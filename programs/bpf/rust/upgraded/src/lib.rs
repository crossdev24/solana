//! @brief Example Rust-based BPF upgraded program

extern crate solana_program;
use solana_program::{
    account_info::AccountInfo, entrypoint, entrypoint::ProgramResult, msg, pubkey::Pubkey,
};

entrypoint!(process_instruction);
fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    _instruction_data: &[u8],
) -> ProgramResult {
    msg!("Upgraded program");
    assert_eq!(accounts.len(), 2);
    Err(43.into())
}
