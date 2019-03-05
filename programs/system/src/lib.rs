use log::*;
use solana_sdk::account::KeyedAccount;
use solana_sdk::native_program::ProgramError;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::solana_entrypoint;
use solana_sdk::system_instruction::SystemInstruction;
use solana_sdk::system_program;

const FROM_ACCOUNT_INDEX: usize = 0;
const TO_ACCOUNT_INDEX: usize = 1;

#[derive(Debug, Clone, PartialEq)]
enum SystemError {
    AccountAlreadyInUse,
    ResultWithNegativeTokens,
    SourceNotSystemAccount,
}

fn create_system_account(
    keyed_accounts: &mut [KeyedAccount],
    tokens: u64,
    space: u64,
    program_id: Pubkey,
) -> Result<(), SystemError> {
    if !system_program::check_id(&keyed_accounts[FROM_ACCOUNT_INDEX].account.owner) {
        info!("CreateAccount: invalid account[from] owner");
        Err(SystemError::SourceNotSystemAccount)?;
    }

    if !keyed_accounts[TO_ACCOUNT_INDEX].account.userdata.is_empty()
        || !system_program::check_id(&keyed_accounts[TO_ACCOUNT_INDEX].account.owner)
    {
        info!(
            "CreateAccount: invalid argument; account {} already in use",
            keyed_accounts[TO_ACCOUNT_INDEX].unsigned_key()
        );
        Err(SystemError::AccountAlreadyInUse)?;
    }
    if tokens > keyed_accounts[FROM_ACCOUNT_INDEX].account.tokens {
        info!(
            "CreateAccount: insufficient tokens ({}, need {})",
            keyed_accounts[FROM_ACCOUNT_INDEX].account.tokens, tokens
        );
        Err(SystemError::ResultWithNegativeTokens)?;
    }
    keyed_accounts[FROM_ACCOUNT_INDEX].account.tokens -= tokens;
    keyed_accounts[TO_ACCOUNT_INDEX].account.tokens += tokens;
    keyed_accounts[TO_ACCOUNT_INDEX].account.owner = program_id;
    keyed_accounts[TO_ACCOUNT_INDEX].account.userdata = vec![0; space as usize];
    keyed_accounts[TO_ACCOUNT_INDEX].account.executable = false;
    Ok(())
}

fn assign_account_to_program(
    keyed_accounts: &mut [KeyedAccount],
    program_id: Pubkey,
) -> Result<(), ProgramError> {
    if !system_program::check_id(&keyed_accounts[FROM_ACCOUNT_INDEX].account.owner) {
        Err(ProgramError::AssignOfUnownedAccount)?;
    }
    keyed_accounts[FROM_ACCOUNT_INDEX].account.owner = program_id;
    Ok(())
}
fn move_tokens(keyed_accounts: &mut [KeyedAccount], tokens: u64) -> Result<(), ProgramError> {
    if tokens > keyed_accounts[FROM_ACCOUNT_INDEX].account.tokens {
        info!(
            "Move: insufficient tokens ({}, need {})",
            keyed_accounts[FROM_ACCOUNT_INDEX].account.tokens, tokens
        );
        Err(ProgramError::ResultWithNegativeTokens)?;
    }
    keyed_accounts[FROM_ACCOUNT_INDEX].account.tokens -= tokens;
    keyed_accounts[TO_ACCOUNT_INDEX].account.tokens += tokens;
    Ok(())
}

solana_entrypoint!(entrypoint);
pub fn entrypoint(
    _program_id: &Pubkey,
    keyed_accounts: &mut [KeyedAccount],
    data: &[u8],
    _tick_height: u64,
) -> Result<(), ProgramError> {
    if let Ok(syscall) = bincode::deserialize(data) {
        trace!("process_instruction: {:?}", syscall);
        trace!("keyed_accounts: {:?}", keyed_accounts);

        // All system instructions require that accounts_keys[0] be a signer
        if keyed_accounts[FROM_ACCOUNT_INDEX].signer_key().is_none() {
            info!("account[from] is unsigned");
            Err(ProgramError::InvalidArgument)?;
        }

        match syscall {
            SystemInstruction::CreateAccount {
                tokens,
                space,
                program_id,
            } => {
                create_system_account(keyed_accounts, tokens, space, program_id).map_err(
                    |e| match e {
                        SystemError::ResultWithNegativeTokens => {
                            ProgramError::ResultWithNegativeTokens
                        }
                        _ => ProgramError::InvalidArgument,
                    },
                )
            }
            SystemInstruction::Assign { program_id } => {
                assign_account_to_program(keyed_accounts, program_id)
            }
            SystemInstruction::Move { tokens } => move_tokens(keyed_accounts, tokens),
        }
    } else {
        info!("Invalid transaction instruction userdata: {:?}", data);
        Err(ProgramError::InvalidUserdata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::account::Account;
    use solana_sdk::signature::{Keypair, KeypairUtil};

    #[test]
    fn test_create_system_account() {
        let new_program_owner = Pubkey::new(&[9; 32]);

        let from = Keypair::new().pubkey();
        let mut from_account = Account::new(100, 0, system_program::id());
        let to = Keypair::new().pubkey();
        let mut to_account = Account::new(0, 0, Pubkey::default());
        let mut keyed_accounts = [
            KeyedAccount::new(&from, true, &mut from_account),
            KeyedAccount::new(&to, false, &mut to_account),
        ];
        create_system_account(&mut keyed_accounts, 50, 2, new_program_owner).unwrap();
        let from_tokens = from_account.tokens;
        let to_tokens = to_account.tokens;
        let to_owner = to_account.owner;
        let to_userdata = to_account.userdata.clone();
        assert_eq!(from_tokens, 50);
        assert_eq!(to_tokens, 50);
        assert_eq!(to_owner, new_program_owner);
        assert_eq!(to_userdata, [0, 0]);

        // Attempt to create account with more tokens than remaining in from_account
        let to = Keypair::new().pubkey();
        let mut to_account = Account::new(0, 0, Pubkey::default());
        let unchanged_account = to_account.clone();
        keyed_accounts = [
            KeyedAccount::new(&from, true, &mut from_account),
            KeyedAccount::new(&to, false, &mut to_account),
        ];
        let result = create_system_account(&mut keyed_accounts, 100, 2, new_program_owner);
        assert_eq!(result, Err(SystemError::ResultWithNegativeTokens));
        let from_tokens = from_account.tokens;
        assert_eq!(from_tokens, 50);
        assert_eq!(to_account, unchanged_account);

        // Attempt to create system account in account already owned by another program
        let original_program_owner = Pubkey::new(&[5; 32]);
        let owned_key = Keypair::new().pubkey();
        let mut owned_account = Account::new(0, 0, original_program_owner);
        let unchanged_account = owned_account.clone();
        keyed_accounts = [
            KeyedAccount::new(&from, true, &mut from_account),
            KeyedAccount::new(&owned_key, false, &mut owned_account),
        ];
        let result = create_system_account(&mut keyed_accounts, 50, 2, new_program_owner);
        assert_eq!(result, Err(SystemError::AccountAlreadyInUse));
        let from_tokens = from_account.tokens;
        assert_eq!(from_tokens, 50);
        assert_eq!(owned_account, unchanged_account);

        // Attempt to create system account in account with populated userdata
        let populated_key = Keypair::new().pubkey();
        let mut populated_account = Account {
            tokens: 0,
            userdata: vec![0, 1, 2, 3],
            owner: Pubkey::default(),
            executable: false,
        };
        let unchanged_account = populated_account.clone();
        keyed_accounts = [
            KeyedAccount::new(&from, true, &mut from_account),
            KeyedAccount::new(&populated_key, false, &mut populated_account),
        ];
        let result = create_system_account(&mut keyed_accounts, 50, 2, new_program_owner);
        assert_eq!(result, Err(SystemError::AccountAlreadyInUse));
        assert_eq!(from_account.tokens, 50);
        assert_eq!(populated_account, unchanged_account);
    }

    #[test]
    fn test_create_not_system_account() {
        let other_program = Pubkey::new(&[9; 32]);

        let from = Keypair::new().pubkey();
        let mut from_account = Account::new(100, 0, other_program);
        let to = Keypair::new().pubkey();
        let mut to_account = Account::new(0, 0, Pubkey::default());
        let mut keyed_accounts = [
            KeyedAccount::new(&from, true, &mut from_account),
            KeyedAccount::new(&to, false, &mut to_account),
        ];
        let result = create_system_account(&mut keyed_accounts, 50, 2, other_program);
        assert_eq!(result, Err(SystemError::SourceNotSystemAccount));
    }

    #[test]
    fn test_assign_account_to_program() {
        let new_program_owner = Pubkey::new(&[9; 32]);

        let from = Keypair::new().pubkey();
        let mut from_account = Account::new(100, 0, system_program::id());
        let mut keyed_accounts = [KeyedAccount::new(&from, true, &mut from_account)];
        assign_account_to_program(&mut keyed_accounts, new_program_owner).unwrap();
        let from_owner = from_account.owner;
        assert_eq!(from_owner, new_program_owner);

        // Attempt to assign account not owned by system program
        let another_program_owner = Pubkey::new(&[8; 32]);
        keyed_accounts = [KeyedAccount::new(&from, true, &mut from_account)];
        let result = assign_account_to_program(&mut keyed_accounts, another_program_owner);
        assert_eq!(result, Err(ProgramError::AssignOfUnownedAccount));
        assert_eq!(from_account.owner, new_program_owner);
    }

    #[test]
    fn test_move_tokens() {
        let from = Keypair::new().pubkey();
        let mut from_account = Account::new(100, 0, Pubkey::new(&[2; 32])); // account owner should not matter
        let to = Keypair::new().pubkey();
        let mut to_account = Account::new(1, 0, Pubkey::new(&[3; 32])); // account owner should not matter
        let mut keyed_accounts = [
            KeyedAccount::new(&from, true, &mut from_account),
            KeyedAccount::new(&to, false, &mut to_account),
        ];
        move_tokens(&mut keyed_accounts, 50).unwrap();
        let from_tokens = from_account.tokens;
        let to_tokens = to_account.tokens;
        assert_eq!(from_tokens, 50);
        assert_eq!(to_tokens, 51);

        // Attempt to move more tokens than remaining in from_account
        keyed_accounts = [
            KeyedAccount::new(&from, true, &mut from_account),
            KeyedAccount::new(&to, false, &mut to_account),
        ];
        let result = move_tokens(&mut keyed_accounts, 100);
        assert_eq!(result, Err(ProgramError::ResultWithNegativeTokens));
        assert_eq!(from_account.tokens, 50);
        assert_eq!(to_account.tokens, 51);
    }
}
