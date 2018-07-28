//! The `entry_writer` module helps implement the TPU's write stage. It
//! writes entries to the given writer, which is typically a file or
//! stdout, and then sends the Entry to its output channel.

use bank::Bank;
use bincode;
use entry::Entry;
use std::io::{self, BufRead, Error, ErrorKind, Write};

pub struct EntryWriter<'a, W> {
    bank: &'a Bank,
    writer: W,
}

impl<'a, W: Write> EntryWriter<'a, W> {
    /// Create a new Tpu that wraps the given Bank.
    pub fn new(bank: &'a Bank, writer: W) -> Self {
        EntryWriter { bank, writer }
    }

    fn write_entry(writer: &mut W, entry: &Entry) -> io::Result<()> {
        let entry_bytes =
            bincode::serialize(&entry).map_err(|e| Error::new(ErrorKind::Other, e.to_string()))?;

        let len = entry_bytes.len();
        bincode::serialize_into(writer, &len)
            .map_err(|e| Error::new(ErrorKind::Other, e.to_string()))?;

        writer.write_all(&entry_bytes[..])?;

        writer.flush()
    }

    pub fn write_entries<I>(writer: &mut W, entries: I) -> io::Result<()>
    where
        I: IntoIterator<Item = Entry>,
    {
        for entry in entries {
            Self::write_entry(writer, &entry)?;
        }
        Ok(())
    }

    fn write_and_register_entry(&mut self, entry: &Entry) -> io::Result<()> {
        trace!("write_and_register_entry entry");
        if !entry.has_more {
            self.bank.register_entry_id(&entry.id);
        }
        Self::write_entry(&mut self.writer, entry)
    }

    pub fn write_and_register_entries(&mut self, entries: &[Entry]) -> io::Result<()> {
        for entry in entries {
            self.write_and_register_entry(&entry)?;
        }
        Ok(())
    }
}

struct EntryReader<R: BufRead> {
    reader: R,
    entry_bytes: Vec<u8>,
    len_len: u64,
}

impl Iterator for EntryReader<BufRead> {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<io::Result<Entry>> {
        let mut entry_len: usize = 0;

        let mut entry_len_bytes = [0u8; 8];

        if let Err(e) = self.reader.read_exact(&mut entry_len_bytes[..self.len_len]) {
            None // EOF, probably
        } else {
            entry_len = bincode::deserialize(&entry_len_bytes).unwrap();

            if entry_len > self.entry_bytes.len() {
                self.entry_bytes.resize(entry_len);
            }

            if let Err(e) = self.reader.read_exact(&mut self.entry_bytes[..entry_len]) {
                Some(Error::new(ErrorKind::Other, e.to_string()))
            } else {
                Some(
                    bincode::deserialize(&self.entry_bytes)
                        .map_err(|e| Error::new(ErrorKind::Other, e.to_string())),
                )
            }
        }
    }
}

/// Return an iterator for all the entries in the given file.
pub fn read_entries<R: BufRead>(reader: R) -> impl Iterator<Item = io::Result<Entry>> {
    let entry_len: usize = 0;

    EntryReader {
        reader,
        entry_bytes: Vec::new(),
        len_len: bincode::serialized_size(&entry_len).unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger;
    use mint::Mint;
    use packet::BLOB_DATA_SIZE;
    use signature::{KeyPair, KeyPairUtil};
    use std::io::Cursor;
    use transaction::Transaction;

    #[test]
    fn test_dont_register_partial_entries() {
        let mint = Mint::new(1);
        let bank = Bank::new(&mint);

        let writer = io::sink();
        let mut entry_writer = EntryWriter::new(&bank, writer);
        let keypair = KeyPair::new();
        let tx = Transaction::new(&mint.keypair(), keypair.pubkey(), 1, mint.last_id());

        // NOTE: if Entry grows to larger than a transaction, the code below falls over
        let threshold = (BLOB_DATA_SIZE / 256) - 1; // 256 is transaction size

        // Verify large entries are split up and the first sets has_more.
        let txs = vec![tx.clone(); threshold * 2];
        let entries = ledger::next_entries(&mint.last_id(), 0, txs);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].has_more);
        assert!(!entries[1].has_more);

        // Verify that write_and_register_entry doesn't register the first entries after a split.
        assert_eq!(bank.last_id(), mint.last_id());
        entry_writer.write_and_register_entry(&entries[0]).unwrap();
        assert_eq!(bank.last_id(), mint.last_id());

        // Verify that write_and_register_entry registers the final entry after a split.
        entry_writer.write_and_register_entry(&entries[1]).unwrap();
        assert_eq!(bank.last_id(), entries[1].id);
    }

    /// Same as read_entries() but parsing a buffer and returning a vector.
    fn read_entries_from_buf(s: &[u8]) -> io::Result<Vec<Entry>> {
        let mut result = vec![];
        let reader = Cursor::new(s);
        for x in read_entries(reader) {
            trace!("entry... {:?}", x);
            result.push(x?);
        }
        Ok(result)
    }

    #[test]
    fn test_read_entries_from_buf() {
        let mint = Mint::new(b'\\' as i64); // guarantee we have a backslash in the buffer
        let mut buf = vec![];
        EntryWriter::write_entries(&mut buf, mint.create_entries()).unwrap();
        let entries = read_entries_from_buf(&buf).unwrap();
        assert_eq!(entries, mint.create_entries());

        let mint = Mint::new(b'\n' as i64); // guarantee we have a newline in the buffer
        let mut buf = vec![];
        EntryWriter::write_entries(&mut buf, mint.create_entries()).unwrap();
        let entries = read_entries_from_buf(&buf).unwrap();
        assert_eq!(entries, mint.create_entries());
    }
}
