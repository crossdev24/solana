//! The `entry` module is a fundamental building block of Proof of History. It contains a
//! unique ID that is the hash of the Entry before it, plus the hash of the
//! transactions within it. Entries cannot be reordered, and its field `num_hashes`
//! represents an approximate amount of time since the last Entry was created.
use bincode::{serialize_into, serialized_size};
use budget_transaction::BudgetTransaction;
use hash::Hash;
use packet::{BlobRecycler, SharedBlob, BLOB_DATA_SIZE};
use poh::Poh;
use rayon::prelude::*;
use signature::Pubkey;
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender};
use transaction::Transaction;

pub type EntrySender = Sender<Vec<Entry>>;
pub type EntryReceiver = Receiver<Vec<Entry>>;

/// Each Entry contains three pieces of data. The `num_hashes` field is the number
/// of hashes performed since the previous entry.  The `id` field is the result
/// of hashing `id` from the previous entry `num_hashes` times.  The `transactions`
/// field points to Transactions that took place shortly before `id` was generated.
///
/// If you divide `num_hashes` by the amount of time it takes to generate a new hash, you
/// get a duration estimate since the last Entry. Since processing power increases
/// over time, one should expect the duration `num_hashes` represents to decrease proportionally.
/// An upper bound on Duration can be estimated by assuming each hash was generated by the
/// world's fastest processor at the time the entry was recorded. Or said another way, it
/// is physically not possible for a shorter duration to have occurred if one assumes the
/// hash was computed by the world's fastest processor at that time. The hash chain is both
/// a Verifiable Delay Function (VDF) and a Proof of Work (not to be confused with Proof of
/// Work consensus!)

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct Entry {
    /// The number of hashes since the previous Entry ID.
    pub num_hashes: u64,

    /// The SHA-256 hash `num_hashes` after the previous Entry ID.
    pub id: Hash,

    /// An unordered list of transactions that were observed before the Entry ID was
    /// generated. They may have been observed before a previous Entry ID but were
    /// pushed back into this list to ensure deterministic interpretation of the ledger.
    pub transactions: Vec<Transaction>,
}

impl Entry {
    /// Creates the next Entry `num_hashes` after `start_hash`.
    pub fn new(start_hash: &Hash, num_hashes: u64, transactions: Vec<Transaction>) -> Self {
        let num_hashes = num_hashes + if transactions.is_empty() { 0 } else { 1 };
        let id = next_hash(start_hash, 0, &transactions);
        let entry = Entry {
            num_hashes,
            id,
            transactions,
        };

        let size = serialized_size(&entry).unwrap();
        if size > BLOB_DATA_SIZE as u64 {
            panic!(
                "Serialized entry size too large: {} ({} transactions):",
                size,
                entry.transactions.len()
            );
        }
        entry
    }

    pub fn to_blob(
        &self,
        blob_recycler: &BlobRecycler,
        idx: Option<u64>,
        id: Option<Pubkey>,
        addr: Option<&SocketAddr>,
    ) -> SharedBlob {
        let blob = blob_recycler.allocate();
        {
            let mut blob_w = blob.write();
            let pos = {
                let mut out = Cursor::new(blob_w.data_mut());
                serialize_into(&mut out, &self).expect("failed to serialize output");
                out.position() as usize
            };
            blob_w.set_size(pos);

            if let Some(idx) = idx {
                blob_w.set_index(idx).expect("set_index()");
            }
            if let Some(id) = id {
                blob_w.set_id(id).expect("set_id()");
            }
            if let Some(addr) = addr {
                blob_w.meta.set_addr(addr);
            }
            blob_w.set_flags(0).unwrap();
        }
        blob
    }

    pub fn will_fit(transactions: Vec<Transaction>) -> bool {
        serialized_size(&Entry {
            num_hashes: 0,
            id: Hash::default(),
            transactions,
        }).unwrap()
            <= BLOB_DATA_SIZE as u64
    }

    pub fn num_will_fit(transactions: &[Transaction]) -> usize {
        if transactions.is_empty() {
            return 0;
        }
        let mut num = transactions.len();
        let mut upper = transactions.len();
        let mut lower = 1; // if one won't fit, we have a lot of TODOs
        let mut next = transactions.len(); // optimistic
        loop {
            debug!(
                "num {}, upper {} lower {} next {} transactions.len() {}",
                num,
                upper,
                lower,
                next,
                transactions.len()
            );
            if Entry::will_fit(transactions[..num].to_vec()) {
                next = (upper + num) / 2;
                lower = num;
                debug!("num {} fits, maybe too well? trying {}", num, next);
            } else {
                next = (lower + num) / 2;
                upper = num;
                debug!("num {} doesn't fit! trying {}", num, next);
            }
            // same as last time
            if next == num {
                debug!("converged on num {}", num);
                break;
            }
            num = next;
        }
        num
    }

    /// Creates the next Tick Entry `num_hashes` after `start_hash`.
    pub fn new_mut(
        start_hash: &mut Hash,
        num_hashes: &mut u64,
        transactions: Vec<Transaction>,
    ) -> Self {
        let entry = Self::new(start_hash, *num_hashes, transactions);
        *start_hash = entry.id;
        *num_hashes = 0;
        assert!(serialized_size(&entry).unwrap() <= BLOB_DATA_SIZE as u64);
        entry
    }

    /// Creates a Entry from the number of hashes `num_hashes` since the previous transaction
    /// and that resulting `id`.
    pub fn new_tick(num_hashes: u64, id: &Hash) -> Self {
        Entry {
            num_hashes,
            id: *id,
            transactions: vec![],
        }
    }

    /// Verifies self.id is the result of hashing a `start_hash` `self.num_hashes` times.
    /// If the transaction is not a Tick, then hash that as well.
    pub fn verify(&self, start_hash: &Hash) -> bool {
        let tx_plans_verified = self.transactions.par_iter().all(|tx| {
            let r = tx.verify_plan();
            if !r {
                warn!("tx plan invalid: {:?}", tx);
            }
            r
        });
        if !tx_plans_verified {
            return false;
        }
        let ref_hash = next_hash(start_hash, self.num_hashes, &self.transactions);
        if self.id != ref_hash {
            warn!(
                "next_hash is invalid expected: {:?} actual: {:?}",
                self.id, ref_hash
            );
            return false;
        }
        true
    }
}

/// Creates the hash `num_hashes` after `start_hash`. If the transaction contains
/// a signature, the final hash will be a hash of both the previous ID and
/// the signature.  If num_hashes is zero and there's no transaction data,
///  start_hash is returned.
fn next_hash(start_hash: &Hash, num_hashes: u64, transactions: &[Transaction]) -> Hash {
    if num_hashes == 0 && transactions.is_empty() {
        return *start_hash;
    }

    let mut poh = Poh::new(*start_hash);

    for _ in 1..num_hashes {
        poh.hash();
    }

    if transactions.is_empty() {
        poh.tick().id
    } else {
        poh.record(Transaction::hash(transactions)).id
    }
}

/// Creates the next Tick or Transaction Entry `num_hashes` after `start_hash`.
pub fn next_entry(start_hash: &Hash, num_hashes: u64, transactions: Vec<Transaction>) -> Entry {
    assert!(num_hashes > 0 || transactions.is_empty());
    Entry {
        num_hashes,
        id: next_hash(start_hash, num_hashes, &transactions),
        transactions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use budget_transaction::BudgetTransaction;
    use chrono::prelude::*;
    use entry::Entry;
    use hash::hash;
    use signature::{Keypair, KeypairUtil};
    use system_transaction::SystemTransaction;
    use transaction::Transaction;

    #[test]
    fn test_entry_verify() {
        let zero = Hash::default();
        let one = hash(&zero.as_ref());
        assert!(Entry::new_tick(0, &zero).verify(&zero)); // base case
        assert!(!Entry::new_tick(0, &zero).verify(&one)); // base case, bad
        assert!(next_entry(&zero, 1, vec![]).verify(&zero)); // inductive step
        assert!(!next_entry(&zero, 1, vec![]).verify(&one)); // inductive step, bad
    }

    #[test]
    fn test_transaction_reorder_attack() {
        let zero = Hash::default();

        // First, verify entries
        let keypair = Keypair::new();
        let tx0 = Transaction::system_new(&keypair, keypair.pubkey(), 0, zero);
        let tx1 = Transaction::system_new(&keypair, keypair.pubkey(), 1, zero);
        let mut e0 = Entry::new(&zero, 0, vec![tx0.clone(), tx1.clone()]);
        assert!(e0.verify(&zero));

        // Next, swap two transactions and ensure verification fails.
        e0.transactions[0] = tx1; // <-- attack
        e0.transactions[1] = tx0;
        assert!(!e0.verify(&zero));
    }

    #[test]
    fn test_witness_reorder_attack() {
        let zero = Hash::default();

        // First, verify entries
        let keypair = Keypair::new();
        let tx0 = Transaction::budget_new_timestamp(
            &keypair,
            keypair.pubkey(),
            keypair.pubkey(),
            Utc::now(),
            zero,
        );
        let tx1 =
            Transaction::budget_new_signature(&keypair, keypair.pubkey(), keypair.pubkey(), zero);
        let mut e0 = Entry::new(&zero, 0, vec![tx0.clone(), tx1.clone()]);
        assert!(e0.verify(&zero));

        // Next, swap two witness transactions and ensure verification fails.
        e0.transactions[0] = tx1; // <-- attack
        e0.transactions[1] = tx0;
        assert!(!e0.verify(&zero));
    }

    #[test]
    fn test_next_entry() {
        let zero = Hash::default();
        let tick = next_entry(&zero, 1, vec![]);
        assert_eq!(tick.num_hashes, 1);
        assert_ne!(tick.id, zero);

        let tick = next_entry(&zero, 0, vec![]);
        assert_eq!(tick.num_hashes, 0);
        assert_eq!(tick.id, zero);

        let keypair = Keypair::new();
        let tx0 = Transaction::budget_new_timestamp(
            &keypair,
            keypair.pubkey(),
            keypair.pubkey(),
            Utc::now(),
            zero,
        );
        let entry0 = next_entry(&zero, 1, vec![tx0.clone()]);
        assert_eq!(entry0.num_hashes, 1);
        assert_eq!(entry0.id, next_hash(&zero, 1, &vec![tx0]));
    }

    #[test]
    #[should_panic]
    fn test_next_entry_panic() {
        let zero = Hash::default();
        let keypair = Keypair::new();
        let tx = Transaction::system_new(&keypair, keypair.pubkey(), 0, zero);
        next_entry(&zero, 0, vec![tx]);
    }
}
