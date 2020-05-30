use crate::{
    config, id,
    stake_state::{Authorized, Lockup, StakeAccount, StakeAuthorize, StakeState},
};
use log::*;
use num_derive::{FromPrimitive, ToPrimitive};
use serde_derive::{Deserialize, Serialize};
use solana_sdk::{
    account::{get_signers, KeyedAccount},
    clock::{Epoch, UnixTimestamp},
    instruction::{AccountMeta, Instruction, InstructionError},
    program_utils::{limited_deserialize, next_keyed_account, DecodeError},
    pubkey::Pubkey,
    system_instruction,
    sysvar::{self, clock::Clock, rent::Rent, stake_history::StakeHistory, Sysvar},
};
use thiserror::Error;

/// Reasons the stake might have had an error
#[derive(Error, Debug, Clone, PartialEq, FromPrimitive, ToPrimitive)]
pub enum StakeError {
    #[error("not enough credits to redeem")]
    NoCreditsToRedeem,

    #[error("lockup has not yet expired")]
    LockupInForce,

    #[error("stake already deactivated")]
    AlreadyDeactivated,

    #[error("one re-delegation permitted per epoch")]
    TooSoonToRedelegate,

    #[error("split amount is more than is staked")]
    InsufficientStake,
}

impl<E> DecodeError<E> for StakeError {
    fn type_of() -> &'static str {
        "StakeError"
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum StakeInstruction {
    /// Initialize a stake with lockup and authorization information
    ///
    /// # Account references
    ///   0. [WRITE] Uninitialized stake account
    ///   1. [] Rent sysvar
    ///
    /// Authorized carries pubkeys that must sign staker transactions
    ///   and withdrawer transactions.
    /// Lockup carries information about withdrawal restrictions
    Initialize(Authorized, Lockup),

    /// Authorize a key to manage stake or withdrawal
    ///
    /// # Account references
    ///   0. [WRITE] Stake account to be updated
    ///   1. [] (reserved for future use) Clock sysvar
    ///   2. [SIGNER] The stake or withdraw authority
    Authorize(Pubkey, StakeAuthorize),

    /// Delegate a stake to a particular vote account
    ///
    /// # Account references
    ///   0. [WRITE] Initialized stake account to be delegated
    ///   1. [] Vote account to which this stake will be delegated
    ///   2. [] Clock sysvar
    ///   3. [] Stake history sysvar that carries stake warmup/cooldown history
    ///   4. [] Address of config account that carries stake config
    ///   5. [SIGNER] Stake authority
    ///
    /// The entire balance of the staking account is staked.  DelegateStake
    ///   can be called multiple times, but re-delegation is delayed
    ///   by one epoch
    DelegateStake,

    /// Split u64 tokens and stake off a stake account into another stake account.
    ///
    /// # Account references
    ///   0. [WRITE] Stake account to be split; must be in the Initialized or Stake state
    ///   1. [WRITE] Uninitialized stake account that will take the split-off amount
    ///   2. [SIGNER] Stake authority
    Split(u64),

    /// Withdraw unstaked lamports from the stake account
    ///
    /// # Account references
    ///   0. [WRITE] Stake account from which to withdraw
    ///   1. [WRITE] Recipient account
    ///   2. [] Clock sysvar
    ///   3. [] Stake history sysvar that carries stake warmup/cooldown history
    ///   4. [SIGNER] Withdraw authority
    ///   5. Optional: [SIGNER] Lockup authority, if before lockup expiration
    ///
    /// The u64 is the portion of the stake account balance to be withdrawn,
    ///    must be `<= StakeAccount.lamports - staked_lamports`.
    Withdraw(u64),

    /// Deactivates the stake in the account
    ///
    /// # Account references
    ///   0. [WRITE] Delegated stake account
    ///   1. [] Clock sysvar
    ///   2. [SIGNER] Stake authority
    Deactivate,

    /// Set stake lockup
    ///
    /// # Account references
    ///   0. [WRITE] Initialized stake account
    ///   1. [SIGNER] Lockup authority
    SetLockup(LockupArgs),
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq, Clone, Copy)]
pub struct LockupArgs {
    pub unix_timestamp: Option<UnixTimestamp>,
    pub epoch: Option<Epoch>,
    pub custodian: Option<Pubkey>,
}

fn initialize(stake_pubkey: &Pubkey, authorized: &Authorized, lockup: &Lockup) -> Instruction {
    Instruction::new(
        id(),
        &StakeInstruction::Initialize(*authorized, *lockup),
        vec![
            AccountMeta::new(*stake_pubkey, false),
            AccountMeta::new_readonly(sysvar::rent::id(), false),
        ],
    )
}

pub fn create_account_with_seed(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    base: &Pubkey,
    seed: &str,
    authorized: &Authorized,
    lockup: &Lockup,
    lamports: u64,
) -> Vec<Instruction> {
    vec![
        system_instruction::create_account_with_seed(
            from_pubkey,
            stake_pubkey,
            base,
            seed,
            lamports,
            std::mem::size_of::<StakeState>() as u64,
            &id(),
        ),
        initialize(stake_pubkey, authorized, lockup),
    ]
}

pub fn create_account(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    authorized: &Authorized,
    lockup: &Lockup,
    lamports: u64,
) -> Vec<Instruction> {
    vec![
        system_instruction::create_account(
            from_pubkey,
            stake_pubkey,
            lamports,
            std::mem::size_of::<StakeState>() as u64,
            &id(),
        ),
        initialize(stake_pubkey, authorized, lockup),
    ]
}

fn _split(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    lamports: u64,
    split_stake_pubkey: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new(*split_stake_pubkey, false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];

    Instruction::new(id(), &StakeInstruction::Split(lamports), account_metas)
}

pub fn split(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    lamports: u64,
    split_stake_pubkey: &Pubkey,
) -> Vec<Instruction> {
    vec![
        system_instruction::create_account(
            authorized_pubkey, // Sending 0, so any signer will suffice
            split_stake_pubkey,
            0,
            std::mem::size_of::<StakeState>() as u64,
            &id(),
        ),
        _split(
            stake_pubkey,
            authorized_pubkey,
            lamports,
            split_stake_pubkey,
        ),
    ]
}

pub fn split_with_seed(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    lamports: u64,
    split_stake_pubkey: &Pubkey, // derived using create_address_with_seed()
    base: &Pubkey,               // base
    seed: &str,                  // seed
) -> Vec<Instruction> {
    vec![
        system_instruction::create_account_with_seed(
            authorized_pubkey, // Sending 0, so any signer will suffice
            split_stake_pubkey,
            base,
            seed,
            0,
            std::mem::size_of::<StakeState>() as u64,
            &id(),
        ),
        _split(
            stake_pubkey,
            authorized_pubkey,
            lamports,
            split_stake_pubkey,
        ),
    ]
}

pub fn create_account_and_delegate_stake(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    vote_pubkey: &Pubkey,
    authorized: &Authorized,
    lockup: &Lockup,
    lamports: u64,
) -> Vec<Instruction> {
    let mut instructions = create_account(from_pubkey, stake_pubkey, authorized, lockup, lamports);
    instructions.push(delegate_stake(
        stake_pubkey,
        &authorized.staker,
        vote_pubkey,
    ));
    instructions
}

pub fn create_account_with_seed_and_delegate_stake(
    from_pubkey: &Pubkey,
    stake_pubkey: &Pubkey,
    base: &Pubkey,
    seed: &str,
    vote_pubkey: &Pubkey,
    authorized: &Authorized,
    lockup: &Lockup,
    lamports: u64,
) -> Vec<Instruction> {
    let mut instructions = create_account_with_seed(
        from_pubkey,
        stake_pubkey,
        base,
        seed,
        authorized,
        lockup,
        lamports,
    );
    instructions.push(delegate_stake(
        stake_pubkey,
        &authorized.staker,
        vote_pubkey,
    ));
    instructions
}

pub fn authorize(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    new_authorized_pubkey: &Pubkey,
    stake_authorize: StakeAuthorize,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(sysvar::clock::id(), false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];

    Instruction::new(
        id(),
        &StakeInstruction::Authorize(*new_authorized_pubkey, stake_authorize),
        account_metas,
    )
}

pub fn delegate_stake(
    stake_pubkey: &Pubkey,
    authorized_pubkey: &Pubkey,
    vote_pubkey: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(*vote_pubkey, false),
        AccountMeta::new_readonly(sysvar::clock::id(), false),
        AccountMeta::new_readonly(sysvar::stake_history::id(), false),
        AccountMeta::new_readonly(crate::config::id(), false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];
    Instruction::new(id(), &StakeInstruction::DelegateStake, account_metas)
}

pub fn withdraw(
    stake_pubkey: &Pubkey,
    withdrawer_pubkey: &Pubkey,
    to_pubkey: &Pubkey,
    lamports: u64,
    custodian_pubkey: Option<&Pubkey>,
) -> Instruction {
    let mut account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new(*to_pubkey, false),
        AccountMeta::new_readonly(sysvar::clock::id(), false),
        AccountMeta::new_readonly(sysvar::stake_history::id(), false),
        AccountMeta::new_readonly(*withdrawer_pubkey, true),
    ];

    if let Some(custodian_pubkey) = custodian_pubkey {
        account_metas.push(AccountMeta::new_readonly(*custodian_pubkey, true));
    }

    Instruction::new(id(), &StakeInstruction::Withdraw(lamports), account_metas)
}

pub fn deactivate_stake(stake_pubkey: &Pubkey, authorized_pubkey: &Pubkey) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(sysvar::clock::id(), false),
        AccountMeta::new_readonly(*authorized_pubkey, true),
    ];
    Instruction::new(id(), &StakeInstruction::Deactivate, account_metas)
}

pub fn set_lockup(
    stake_pubkey: &Pubkey,
    lockup: &LockupArgs,
    custodian_pubkey: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*stake_pubkey, false),
        AccountMeta::new_readonly(*custodian_pubkey, true),
    ];
    Instruction::new(id(), &StakeInstruction::SetLockup(*lockup), account_metas)
}

pub fn process_instruction(
    _program_id: &Pubkey,
    keyed_accounts: &[KeyedAccount],
    data: &[u8],
) -> Result<(), InstructionError> {
    trace!("process_instruction: {:?}", data);
    trace!("keyed_accounts: {:?}", keyed_accounts);

    let signers = get_signers(keyed_accounts);

    let keyed_accounts = &mut keyed_accounts.iter();
    let me = &next_keyed_account(keyed_accounts)?;

    match limited_deserialize(data)? {
        StakeInstruction::Initialize(authorized, lockup) => me.initialize(
            &authorized,
            &lockup,
            &Rent::from_keyed_account(next_keyed_account(keyed_accounts)?)?,
        ),
        StakeInstruction::Authorize(authorized_pubkey, stake_authorize) => {
            me.authorize(&authorized_pubkey, stake_authorize, &signers)
        }
        StakeInstruction::DelegateStake => {
            let vote = next_keyed_account(keyed_accounts)?;

            me.delegate(
                &vote,
                &Clock::from_keyed_account(next_keyed_account(keyed_accounts)?)?,
                &StakeHistory::from_keyed_account(next_keyed_account(keyed_accounts)?)?,
                &config::from_keyed_account(next_keyed_account(keyed_accounts)?)?,
                &signers,
            )
        }
        StakeInstruction::Split(lamports) => {
            let split_stake = &next_keyed_account(keyed_accounts)?;
            me.split(lamports, split_stake, &signers)
        }

        StakeInstruction::Withdraw(lamports) => {
            let to = &next_keyed_account(keyed_accounts)?;
            me.withdraw(
                lamports,
                to,
                &Clock::from_keyed_account(next_keyed_account(keyed_accounts)?)?,
                &StakeHistory::from_keyed_account(next_keyed_account(keyed_accounts)?)?,
                &signers,
            )
        }
        StakeInstruction::Deactivate => me.deactivate(
            &Clock::from_keyed_account(next_keyed_account(keyed_accounts)?)?,
            &signers,
        ),

        StakeInstruction::SetLockup(lockup) => me.set_lockup(&lockup, &signers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bincode::serialize;
    use solana_sdk::{account::Account, rent::Rent, sysvar::stake_history::StakeHistory};
    use std::cell::RefCell;

    fn create_default_account() -> RefCell<Account> {
        RefCell::new(Account::default())
    }

    fn process_instruction(instruction: &Instruction) -> Result<(), InstructionError> {
        let accounts: Vec<_> = instruction
            .accounts
            .iter()
            .map(|meta| {
                RefCell::new(if sysvar::clock::check_id(&meta.pubkey) {
                    sysvar::clock::Clock::default().create_account(1)
                } else if sysvar::rewards::check_id(&meta.pubkey) {
                    sysvar::rewards::create_account(1, 0.0)
                } else if sysvar::stake_history::check_id(&meta.pubkey) {
                    sysvar::stake_history::create_account(1, &StakeHistory::default())
                } else if config::check_id(&meta.pubkey) {
                    config::create_account(0, &config::Config::default())
                } else if sysvar::rent::check_id(&meta.pubkey) {
                    sysvar::rent::create_account(1, &Rent::default())
                } else {
                    Account::default()
                })
            })
            .collect();

        {
            let keyed_accounts: Vec<_> = instruction
                .accounts
                .iter()
                .zip(accounts.iter())
                .map(|(meta, account)| KeyedAccount::new(&meta.pubkey, meta.is_signer, account))
                .collect();
            super::process_instruction(&Pubkey::default(), &keyed_accounts, &instruction.data)
        }
    }

    #[test]
    fn test_stake_process_instruction() {
        assert_eq!(
            process_instruction(&initialize(
                &Pubkey::default(),
                &Authorized::default(),
                &Lockup::default()
            )),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&authorize(
                &Pubkey::default(),
                &Pubkey::default(),
                &Pubkey::default(),
                StakeAuthorize::Staker
            )),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(
                &split(
                    &Pubkey::default(),
                    &Pubkey::default(),
                    100,
                    &Pubkey::default()
                )[1]
            ),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(
                &split_with_seed(
                    &Pubkey::default(),
                    &Pubkey::default(),
                    100,
                    &Pubkey::default(),
                    &Pubkey::default(),
                    "seed"
                )[1]
            ),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&delegate_stake(
                &Pubkey::default(),
                &Pubkey::default(),
                &Pubkey::default()
            )),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&withdraw(
                &Pubkey::default(),
                &Pubkey::default(),
                &Pubkey::new_rand(),
                100,
                None,
            )),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&deactivate_stake(&Pubkey::default(), &Pubkey::default())),
            Err(InstructionError::InvalidAccountData),
        );
        assert_eq!(
            process_instruction(&set_lockup(
                &Pubkey::default(),
                &LockupArgs::default(),
                &Pubkey::default()
            )),
            Err(InstructionError::InvalidAccountData),
        );
    }

    #[test]
    fn test_stake_process_instruction_decode_bail() {
        // these will not call stake_state, have bogus contents

        // gets the "is_empty()" check
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[],
                &serialize(&StakeInstruction::Initialize(
                    Authorized::default(),
                    Lockup::default()
                ))
                .unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // no account for rent
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[KeyedAccount::new(
                    &Pubkey::default(),
                    false,
                    &create_default_account(),
                )],
                &serialize(&StakeInstruction::Initialize(
                    Authorized::default(),
                    Lockup::default()
                ))
                .unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // rent fails to deserialize
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&Pubkey::default(), false, &create_default_account(),),
                    KeyedAccount::new(&sysvar::rent::id(), false, &create_default_account(),)
                ],
                &serialize(&StakeInstruction::Initialize(
                    Authorized::default(),
                    Lockup::default()
                ))
                .unwrap(),
            ),
            Err(InstructionError::InvalidArgument),
        );

        // fails to deserialize stake state
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&Pubkey::default(), false, &create_default_account()),
                    KeyedAccount::new(
                        &sysvar::rent::id(),
                        false,
                        &RefCell::new(sysvar::rent::create_account(0, &Rent::default()))
                    )
                ],
                &serialize(&StakeInstruction::Initialize(
                    Authorized::default(),
                    Lockup::default()
                ))
                .unwrap(),
            ),
            Err(InstructionError::InvalidAccountData),
        );

        // gets the first check in delegate, wrong number of accounts
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[KeyedAccount::new(
                    &Pubkey::default(),
                    false,
                    &create_default_account()
                ),],
                &serialize(&StakeInstruction::DelegateStake).unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // gets the sub-check for number of args
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[KeyedAccount::new(
                    &Pubkey::default(),
                    false,
                    &create_default_account()
                )],
                &serialize(&StakeInstruction::DelegateStake).unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // gets the check non-deserialize-able account in delegate_stake
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&Pubkey::default(), true, &create_default_account()),
                    KeyedAccount::new(&Pubkey::default(), false, &create_default_account()),
                    KeyedAccount::new(
                        &sysvar::clock::id(),
                        false,
                        &RefCell::new(sysvar::clock::Clock::default().create_account(1))
                    ),
                    KeyedAccount::new(
                        &sysvar::stake_history::id(),
                        false,
                        &RefCell::new(
                            sysvar::stake_history::StakeHistory::default().create_account(1)
                        )
                    ),
                    KeyedAccount::new(
                        &config::id(),
                        false,
                        &RefCell::new(config::create_account(0, &config::Config::default()))
                    ),
                ],
                &serialize(&StakeInstruction::DelegateStake).unwrap(),
            ),
            Err(InstructionError::InvalidAccountData),
        );

        // Tests 3rd keyed account is of correct type (Clock instead of rewards) in withdraw
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&Pubkey::default(), false, &create_default_account()),
                    KeyedAccount::new(&Pubkey::default(), false, &create_default_account()),
                    KeyedAccount::new(
                        &sysvar::rewards::id(),
                        false,
                        &RefCell::new(sysvar::rewards::create_account(1, 0.0))
                    ),
                    KeyedAccount::new(
                        &sysvar::stake_history::id(),
                        false,
                        &RefCell::new(sysvar::stake_history::create_account(
                            1,
                            &StakeHistory::default()
                        ))
                    ),
                ],
                &serialize(&StakeInstruction::Withdraw(42)).unwrap(),
            ),
            Err(InstructionError::InvalidArgument),
        );

        // Tests correct number of accounts are provided in withdraw
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[KeyedAccount::new(
                    &Pubkey::default(),
                    false,
                    &create_default_account()
                )],
                &serialize(&StakeInstruction::Withdraw(42)).unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );

        // Tests 2nd keyed account is of correct type (Clock instead of rewards) in deactivate
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[
                    KeyedAccount::new(&Pubkey::default(), false, &create_default_account()),
                    KeyedAccount::new(
                        &sysvar::rewards::id(),
                        false,
                        &RefCell::new(sysvar::rewards::create_account(1, 0.0))
                    ),
                ],
                &serialize(&StakeInstruction::Deactivate).unwrap(),
            ),
            Err(InstructionError::InvalidArgument),
        );

        // Tests correct number of accounts are provided in deactivate
        assert_eq!(
            super::process_instruction(
                &Pubkey::default(),
                &[],
                &serialize(&StakeInstruction::Deactivate).unwrap(),
            ),
            Err(InstructionError::NotEnoughAccountKeys),
        );
    }

    #[test]
    fn test_custom_error_decode() {
        use num_traits::FromPrimitive;
        fn pretty_err<T>(err: InstructionError) -> String
        where
            T: 'static + std::error::Error + DecodeError<T> + FromPrimitive,
        {
            if let InstructionError::Custom(code) = err {
                let specific_error: T = T::decode_custom_error_to_enum(code).unwrap();
                format!(
                    "{:?}: {}::{:?} - {}",
                    err,
                    T::type_of(),
                    specific_error,
                    specific_error,
                )
            } else {
                "".to_string()
            }
        }
        assert_eq!(
            "Custom(0): StakeError::NoCreditsToRedeem - not enough credits to redeem",
            pretty_err::<StakeError>(StakeError::NoCreditsToRedeem.into())
        )
    }
}
