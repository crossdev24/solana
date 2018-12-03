use budget_program;
use native_loader;
use solana_sdk::account::{create_keyed_accounts, Account, KeyedAccount};
use solana_sdk::native_program::ProgramError;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use storage_program;
use system_program;
use vote_program;

/// Reasons the runtime might have rejected a transaction.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum RuntimeError {
    /// Executing the instruction at the given index produced an error.
    ProgramError(u8, ProgramError),
}

pub fn is_legacy_program(program_id: &Pubkey) -> bool {
    system_program::check_id(program_id)
        || budget_program::check_id(program_id)
        || storage_program::check_id(program_id)
        || vote_program::check_id(program_id)
}

/// Process an instruction
/// This method calls the instruction's program entrypoint method
fn process_instruction(
    tx: &Transaction,
    instruction_index: usize,
    executable_accounts: &mut [(Pubkey, Account)],
    program_accounts: &mut [&mut Account],
    tick_height: u64,
) -> Result<(), ProgramError> {
    let program_id = tx.program_id(instruction_index);

    // Call the program method
    // It's up to the program to implement its own rules on moving funds
    if is_legacy_program(&program_id) {
        if system_program::check_id(&program_id) {
            system_program::process(&tx, instruction_index, program_accounts)?;
        } else if budget_program::check_id(&program_id) {
            budget_program::process(&tx, instruction_index, program_accounts)?;
        } else if storage_program::check_id(&program_id) {
            storage_program::process(&tx, instruction_index, program_accounts)?;
        } else if vote_program::check_id(&program_id) {
            vote_program::process(&tx, instruction_index, program_accounts)?;
        } else {
            unreachable!();
        };
    } else {
        let mut keyed_accounts = create_keyed_accounts(executable_accounts);
        let mut keyed_accounts2: Vec<_> = tx.instructions[instruction_index]
            .accounts
            .iter()
            .map(|&index| {
                let index = index as usize;
                let key = &tx.account_keys[index];
                (key, index < tx.signatures.len())
            }).zip(program_accounts.iter_mut())
            .map(|((key, is_signer), account)| KeyedAccount::new(key, is_signer, account))
            .collect();
        keyed_accounts.append(&mut keyed_accounts2);

        if !native_loader::process_instruction(
            &program_id,
            &mut keyed_accounts,
            &tx.instructions[instruction_index].userdata,
            tick_height,
        ) {
            return Err(ProgramError::GenericError);
        }
    }
    Ok(())
}

fn verify_instruction(
    program_id: &Pubkey,
    pre_program_id: &Pubkey,
    pre_tokens: u64,
    account: &Account,
) -> Result<(), ProgramError> {
    // Verify the transaction

    // Make sure that program_id is still the same or this was just assigned by the system program
    if *pre_program_id != account.owner && !system_program::check_id(&program_id) {
        return Err(ProgramError::ModifiedProgramId);
    }
    // For accounts unassigned to the program, the individual balance of each accounts cannot decrease.
    if *program_id != account.owner && pre_tokens > account.tokens {
        return Err(ProgramError::ExternalAccountTokenSpend);
    }
    Ok(())
}

/// Execute an instruction
/// This method calls the instruction's program entrypoint method and verifies that the result of
/// the call does not violate the bank's accounting rules.
/// The accounts are committed back to the bank only if this function returns Ok(_).
fn execute_instruction(
    tx: &Transaction,
    instruction_index: usize,
    executable_accounts: &mut [(Pubkey, Account)],
    program_accounts: &mut [&mut Account],
    tick_height: u64,
) -> Result<(), ProgramError> {
    let program_id = tx.program_id(instruction_index);
    // TODO: the runtime should be checking read/write access to memory
    // we are trusting the hard-coded programs not to clobber or allocate
    let pre_total: u64 = program_accounts.iter().map(|a| a.tokens).sum();
    let pre_data: Vec<_> = program_accounts
        .iter_mut()
        .map(|a| (a.owner, a.tokens))
        .collect();

    process_instruction(
        tx,
        instruction_index,
        executable_accounts,
        program_accounts,
        tick_height,
    )?;

    // Verify the instruction
    for ((pre_program_id, pre_tokens), post_account) in pre_data.iter().zip(program_accounts.iter())
    {
        verify_instruction(&program_id, pre_program_id, *pre_tokens, post_account)?;
    }
    // The total sum of all the tokens in all the accounts cannot change.
    let post_total: u64 = program_accounts.iter().map(|a| a.tokens).sum();
    if pre_total != post_total {
        return Err(ProgramError::UnbalancedInstruction);
    }
    Ok(())
}

/// Execute a function with a subset of accounts as writable references.
/// Since the subset can point to the same references, in any order there is no way
/// for the borrow checker to track them with regards to the original set.
fn with_subset<F, A>(accounts: &mut [Account], ixes: &[u8], func: F) -> A
where
    F: FnOnce(&mut [&mut Account]) -> A,
{
    let mut subset: Vec<&mut Account> = ixes
        .iter()
        .map(|ix| {
            let ptr = &mut accounts[*ix as usize] as *mut Account;
            // lifetime of this unsafe is only within the scope of the closure
            // there is no way to reorder them without breaking borrow checker rules
            unsafe { &mut *ptr }
        }).collect();
    func(&mut subset)
}

/// Execute a transaction.
/// This method calls each instruction in the transaction over the set of loaded Accounts
/// The accounts are committed back to the bank only if every instruction succeeds
pub fn execute_transaction(
    tx: &Transaction,
    loaders: &mut [Vec<(Pubkey, Account)>],
    tx_accounts: &mut [Account],
    tick_height: u64,
) -> Result<(), RuntimeError> {
    for (instruction_index, instruction) in tx.instructions.iter().enumerate() {
        let executable_accounts = &mut (&mut loaders[instruction.program_ids_index as usize]);
        with_subset(tx_accounts, &instruction.accounts, |program_accounts| {
            execute_instruction(
                tx,
                instruction_index,
                executable_accounts,
                program_accounts,
                tick_height,
            ).map_err(|err| RuntimeError::ProgramError(instruction_index as u8, err))?;
            Ok(())
        })?;
    }
    Ok(())
}
