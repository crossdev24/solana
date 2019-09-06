#[macro_use]
extern crate cfg_if;

pub mod clock;
pub mod pubkey;

// On-chain program modules
cfg_if! {
    if #[cfg(feature = "program")] {
        pub mod entrypoint;
        pub mod log;
        pub mod program_test;
    }
}

// Kitchen sink modules
cfg_if! {
    if #[cfg(feature = "kitchen_sink")] {
        pub mod account;
        pub mod account_utils;
        pub mod bpf_loader;
        pub mod client;
        pub mod fee_calculator;
        pub mod genesis_block;
        pub mod hash;
        pub mod inflation;
        pub mod instruction;
        pub mod instruction_processor_utils;
        pub mod loader_instruction;
        pub mod message;
        pub mod native_loader;
        pub mod packet;
        pub mod poh_config;
        pub mod rent;
        pub mod rpc_port;
        pub mod short_vec;
        pub mod signature;
        pub mod system_instruction;
        pub mod system_program;
        pub mod system_transaction;
        pub mod sysvar;
        pub mod timing;
        pub mod transaction;
        pub mod transport;
    }
}

#[macro_use]
extern crate serde_derive;
