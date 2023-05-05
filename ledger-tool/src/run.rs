use {
    crate::{args::*, ledger_utils::*},
    clap::{value_t, App, Arg, ArgMatches, SubCommand},
    log::*,
    serde::{Deserialize, Serialize},
    serde_json::Result,
    solana_bpf_loader_program::{
        create_vm, load_program_from_bytes, serialization::serialize_parameters,
        syscalls::create_loader,
    },
    solana_clap_utils::input_parsers::pubkeys_of,
    solana_ledger::{
        blockstore_options::{AccessType, BlockstoreRecoveryMode},
        blockstore_processor::ProcessOptions,
    },
    solana_program_runtime::{
        invoke_context::InvokeContext,
        loaded_programs::{LoadProgramMetrics, LoadedProgram, LoadedProgramType},
        with_mock_invoke_context,
    },
    solana_rbpf::{
        assembler::assemble, elf::Executable, static_analysis::Analysis,
        verifier::RequisiteVerifier, vm::VerifiedExecutable,
    },
    solana_runtime::{bank::Bank, runtime_config::RuntimeConfig},
    solana_sdk::{
        account::AccountSharedData,
        account_utils::StateMut,
        bpf_loader_upgradeable::{self, UpgradeableLoaderState},
        pubkey::Pubkey,
        slot_history::Slot,
        transaction_context::{IndexOfAccount, InstructionAccount},
    },
    std::{
        collections::HashSet,
        fmt::{Debug, Formatter},
        fs::File,
        io::{Read, Seek, Write},
        path::{Path, PathBuf},
        process::exit,
        sync::Arc,
        time::{Duration, Instant},
    },
};

#[derive(Serialize, Deserialize, Debug)]
struct Account {
    key: String,
    owner: Option<String>,
    is_signer: Option<bool>,
    is_writable: Option<bool>,
    lamports: Option<u64>,
    data: Option<Vec<u8>>,
}
#[derive(Serialize, Deserialize)]
struct Input {
    program_id: String,
    accounts: Vec<Account>,
    instruction_data: Vec<u8>,
}
fn load_accounts(path: &Path) -> Result<Input> {
    let file = File::open(path).unwrap();
    let input: Input = serde_json::from_reader(file)?;
    info!("Program input:");
    info!("program_id: {}", &input.program_id);
    info!("accounts {:?}", &input.accounts);
    info!("instruction_data {:?}", &input.instruction_data);
    info!("----------------------------------------");
    Ok(input)
}

fn load_blockstore(ledger_path: &Path, arg_matches: &ArgMatches<'_>) -> Arc<Bank> {
    let debug_keys = pubkeys_of(arg_matches, "debug_key")
        .map(|pubkeys| Arc::new(pubkeys.into_iter().collect::<HashSet<_>>()));
    let force_update_to_open = arg_matches.is_present("force_update_to_open");
    let process_options = ProcessOptions {
        new_hard_forks: hardforks_of(arg_matches, "hard_forks"),
        run_verification: false,
        on_halt_store_hash_raw_data_for_debug: false,
        run_final_accounts_hash_calc: false,
        halt_at_slot: value_t!(arg_matches, "halt_at_slot", Slot).ok(),
        debug_keys,
        limit_load_slot_count_from_snapshot: value_t!(
            arg_matches,
            "limit_load_slot_count_from_snapshot",
            usize
        )
        .ok(),
        accounts_db_config: Some(get_accounts_db_config(ledger_path, arg_matches)),
        verify_index: false,
        allow_dead_slots: arg_matches.is_present("allow_dead_slots"),
        accounts_db_test_hash_calculation: false,
        accounts_db_skip_shrink: arg_matches.is_present("accounts_db_skip_shrink"),
        runtime_config: RuntimeConfig::default(),
        ..ProcessOptions::default()
    };
    let snapshot_archive_path = value_t!(arg_matches, "snapshot_archive_path", String)
        .ok()
        .map(PathBuf::from);
    let incremental_snapshot_archive_path =
        value_t!(arg_matches, "incremental_snapshot_archive_path", String)
            .ok()
            .map(PathBuf::from);

    let wal_recovery_mode = arg_matches
        .value_of("wal_recovery_mode")
        .map(BlockstoreRecoveryMode::from);
    let genesis_config = open_genesis_config_by(ledger_path, arg_matches);
    info!("genesis hash: {}", genesis_config.hash());
    let blockstore = open_blockstore(
        ledger_path,
        AccessType::Secondary,
        wal_recovery_mode,
        force_update_to_open,
    );
    let (bank_forks, ..) = load_bank_forks(
        arg_matches,
        &genesis_config,
        Arc::new(blockstore),
        process_options,
        snapshot_archive_path,
        incremental_snapshot_archive_path,
    )
    .unwrap_or_else(|err| {
        eprintln!("Ledger loading failed: {err:?}");
        exit(1);
    });
    let bank = bank_forks.read().unwrap().working_bank();
    bank
}

pub trait RunSubCommand {
    fn run_subcommand(self) -> Self;
}

impl RunSubCommand for App<'_, '_> {
    fn run_subcommand(self) -> Self {
        self.subcommand(
            SubCommand::with_name("run")
        .about(
            r##"Run to test, debug, and analyze on-chain programs.

The tool executes on-chain programs in a mocked environment.

The input data for a program execution have to be in JSON format
and the following fields are required
{
    "program_id": "DozgQiYtGbdyniV2T74xMdmjZJvYDzoRFFqw7UR5MwPK",
    "accounts": [
        {
            "key": "524HMdYYBy6TAn4dK5vCcjiTmT2sxV6Xoue5EXrz22Ca",
            "owner": "BPFLoaderUpgradeab1e11111111111111111111111",
            "is_signer": false,
            "is_writable": true,
            "lamports": 1000,
            "data": [0, 0, 0, 3]
        }
    ],
    "instruction_data": [31, 32, 23, 24]
}
"##,
        )
        .arg(
            Arg::with_name("PROGRAM")
                .help(
                    "Program file to use. This is either an ELF shared-object file to be executed, \
                     or an assembly file to be assembled and executed.",
                )
                .required(true)
                .index(1)
        )
        .arg(
            Arg::with_name("input")
                .help(
                    "Input for the program to run on, where FILE is a name of a JSON file \
with input data, or BYTES is the number of 0-valued bytes to allocate for program parameters",
                )
                .short("i")
                .long("input")
                .value_name("FILE / BYTES")
                .takes_value(true)
                .default_value("0"),
        )
        .arg(
            Arg::with_name("memory")
                .help("Heap memory for the program to run on")
                .short("m")
                .long("memory")
                .value_name("BYTES")
                .takes_value(true)
                .default_value("0"),
        )
        .arg(
            Arg::with_name("use")
                .help(
                    "Method of execution to use, where 'cfg' generates Control Flow Graph \
of the program, 'disassembler' dumps disassembled code of the program, 'interpreter' runs \
the program in the virtual machine's interpreter, 'debugger' is the same as 'interpreter' \
but hosts a GDB interface, and 'jit' precompiles the program to native machine code \
before execting it in the virtual machine.",
                )
                .short("u")
                .long("use")
                .takes_value(true)
                .value_name("VALUE")
                .possible_values(&["cfg", "disassembler", "interpreter", "debugger", "jit"])
                .default_value("jit"),
        )
        .arg(
            Arg::with_name("instruction limit")
                .help("Limit the number of instructions to execute")
                .long("limit")
                .takes_value(true)
                .value_name("COUNT")
                .default_value("9223372036854775807"),
        )
        .arg(
            Arg::with_name("max_genesis_archive_unpacked_size")
                .long("max-genesis-archive-unpacked-size")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value("10485760")
                .help("maximum total uncompressed size of unpacked genesis archive")
        )
        .arg(
            Arg::with_name("port")
                .help("Port to use for the connection with a remote debugger")
                .long("port")
                .takes_value(true)
                .value_name("PORT")
                .default_value("9001"),
        )
        .arg(
            Arg::with_name("trace")
                .help("Output instruction trace")
                .short("t")
                .long("trace")
                .takes_value(true)
                .value_name("FILE"),
        )
        )
    }
}

#[derive(Serialize)]
struct Output {
    result: String,
    instruction_count: u64,
    execution_time: Duration,
    log: Vec<String>,
}

impl Debug for Output {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Result: {}", self.result)?;
        writeln!(f, "Instruction Count: {}", self.instruction_count)?;
        writeln!(f, "Execution time: {} us", self.execution_time.as_micros())?;
        for line in &self.log {
            writeln!(f, "{line}")?;
        }
        Ok(())
    }
}

// Replace with std::lazy::Lazy when stabilized.
// https://github.com/rust-lang/rust/issues/74465
struct LazyAnalysis<'a, 'b> {
    analysis: Option<Analysis<'a>>,
    executable: &'a Executable<InvokeContext<'b>>,
}

impl<'a, 'b> LazyAnalysis<'a, 'b> {
    fn new(executable: &'a Executable<InvokeContext<'b>>) -> Self {
        Self {
            analysis: None,
            executable,
        }
    }

    fn analyze(&mut self) -> &Analysis {
        if let Some(ref analysis) = self.analysis {
            return analysis;
        }
        self.analysis
            .insert(Analysis::from_executable(self.executable).unwrap())
    }
}

fn output_trace(
    matches: &ArgMatches<'_>,
    trace: &[[u64; 12]],
    frame: usize,
    analysis: &mut LazyAnalysis,
) {
    if matches.value_of("trace").unwrap() == "stdout" {
        writeln!(&mut std::io::stdout(), "Frame {frame}").unwrap();
        analysis
            .analyze()
            .disassemble_trace_log(&mut std::io::stdout(), trace)
            .unwrap();
    } else {
        let filename = format!("{}.{}", matches.value_of("trace").unwrap(), frame);
        let mut fd = File::create(filename).unwrap();
        writeln!(&fd, "Frame {frame}").unwrap();
        analysis
            .analyze()
            .disassemble_trace_log(&mut fd, trace)
            .unwrap();
    }
}

pub fn run(ledger_path: &Path, matches: &ArgMatches<'_>) {
    let bank = load_blockstore(ledger_path, matches);
    let loader_id = bpf_loader_upgradeable::id();
    let mut transaction_accounts = Vec::new();
    let mut instruction_accounts = Vec::new();
    let mut program_id = Pubkey::new_unique();
    let mut cached_account_keys = vec![];

    let instruction_data = match matches.value_of("input").unwrap().parse::<usize>() {
        Ok(allocation_size) => {
            let pubkey = Pubkey::new_unique();
            transaction_accounts.push((
                pubkey,
                AccountSharedData::new(0, allocation_size, &Pubkey::new_unique()),
            ));
            instruction_accounts.push(InstructionAccount {
                index_in_transaction: 0,
                index_in_caller: 0,
                index_in_callee: 0,
                is_signer: false,
                is_writable: true,
            });
            vec![]
        }
        Err(_) => {
            let input = load_accounts(Path::new(matches.value_of("input").unwrap())).unwrap();
            program_id = input.program_id.parse::<Pubkey>().unwrap_or_else(|err| {
                eprintln!(
                    "Invalid program ID in input {}, error {}",
                    input.program_id, err,
                );
                program_id
            });
            for (index, account_info) in input.accounts.into_iter().enumerate() {
                let pubkey = account_info.key.parse::<Pubkey>().unwrap_or_else(|err| {
                    eprintln!("Invalid key in input {}, error {}", account_info.key, err);
                    exit(1);
                });
                let data = account_info.data.unwrap_or(vec![]);
                let space = data.len();
                let account = if let Some(account) = bank.get_account_with_fixed_root(&pubkey) {
                    let owner = *account.owner();
                    if bpf_loader_upgradeable::check_id(&owner) {
                        if let Ok(UpgradeableLoaderState::Program {
                            programdata_address,
                        }) = account.state()
                        {
                            debug!("Program data address {}", programdata_address);
                            if bank
                                .get_account_with_fixed_root(&programdata_address)
                                .is_some()
                            {
                                cached_account_keys.push(pubkey);
                            }
                        }
                    }
                    // Override account data and lamports from input file if provided
                    if space > 0 {
                        let lamports = account_info.lamports.unwrap_or(account.lamports());
                        let mut account = AccountSharedData::new(lamports, space, &owner);
                        account.set_data(data);
                        account
                    } else {
                        account
                    }
                } else {
                    let owner = account_info
                        .owner
                        .unwrap_or(Pubkey::new_unique().to_string());
                    let owner = owner.parse::<Pubkey>().unwrap_or_else(|err| {
                        eprintln!("Invalid owner key in input {owner}, error {err}");
                        Pubkey::new_unique()
                    });
                    let lamports = account_info.lamports.unwrap_or(0);
                    let mut account = AccountSharedData::new(lamports, space, &owner);
                    account.set_data(data);
                    account
                };
                transaction_accounts.push((pubkey, account));
                instruction_accounts.push(InstructionAccount {
                    index_in_transaction: index as IndexOfAccount,
                    index_in_caller: index as IndexOfAccount,
                    index_in_callee: index as IndexOfAccount,
                    is_signer: account_info.is_signer.unwrap_or(false),
                    is_writable: account_info.is_writable.unwrap_or(false),
                });
            }
            input.instruction_data
        }
    };
    let program_index: u16 = instruction_accounts.len().try_into().unwrap();
    transaction_accounts.push((
        loader_id,
        AccountSharedData::new(0, 0, &solana_sdk::native_loader::id()),
    ));
    transaction_accounts.push((
        program_id, // ID of the loaded program. It can modify accounts with the same owner key
        AccountSharedData::new(0, 0, &loader_id),
    ));
    let interpreted = matches.value_of("use").unwrap() != "jit";
    with_mock_invoke_context!(
        invoke_context,
        transaction_context,
        transaction_accounts,
        bank.get_builtin_programs()
    );

    for key in cached_account_keys {
        let program = bank.load_program(&key, true).unwrap_or_else(|err| {
            // Create a tombstone for the program in the cache
            debug!("Failed to load program {}, error {:?}", key, err);
            Arc::new(LoadedProgram::new_tombstone(
                0,
                LoadedProgramType::FailedVerification,
            ))
        });
        debug!("Loaded program {}", key);
        invoke_context
            .tx_executor_cache
            .borrow_mut()
            .set(key, program, false, false, 0);
    }

    invoke_context
        .transaction_context
        .get_next_instruction_context()
        .unwrap()
        .configure(
            &[program_index, program_index.saturating_add(1)],
            &instruction_accounts,
            &instruction_data,
        );
    invoke_context.push().unwrap();
    let (_parameter_bytes, regions, account_lengths) = serialize_parameters(
        invoke_context.transaction_context,
        invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap(),
        true, // should_cap_ix_accounts
        true, // copy_account_data
    )
    .unwrap();

    let program = matches.value_of("PROGRAM").unwrap();
    let mut file = File::open(Path::new(program)).unwrap();
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).unwrap();
    file.rewind().unwrap();
    let mut contents = Vec::new();
    file.read_to_end(&mut contents).unwrap();

    let slot = Slot::default();
    let reject_deployment_of_broken_elfs = false;
    let debugging_features = true;
    let log_collector = invoke_context.get_log_collector();
    let loader_key = bpf_loader_upgradeable::id();
    let mut load_program_metrics = LoadProgramMetrics {
        program_id: program_id.to_string(),
        ..LoadProgramMetrics::default()
    };
    let account_size = contents.len();
    #[allow(unused_mut)]
    let mut verified_executable = if magic == [0x7f, 0x45, 0x4c, 0x46] {
        let result = load_program_from_bytes(
            &invoke_context.feature_set,
            invoke_context.get_compute_budget(),
            log_collector,
            &mut load_program_metrics,
            &contents,
            &loader_key,
            account_size,
            slot,
            reject_deployment_of_broken_elfs,
            debugging_features,
        );
        match result {
            Ok(loaded_program) => match loaded_program.program {
                LoadedProgramType::LegacyV1(program) => Ok(unsafe { std::mem::transmute(program) }),
                _ => unreachable!(),
            },
            Err(err) => Err(format!("Loading executable failed: {err:?}")),
        }
    } else {
        let loader = create_loader(
            &invoke_context.feature_set,
            invoke_context.get_compute_budget(),
            true,
            true,
            true,
        )
        .unwrap();
        let executable =
            assemble::<InvokeContext>(std::str::from_utf8(contents.as_slice()).unwrap(), loader)
                .unwrap();
        VerifiedExecutable::<RequisiteVerifier, InvokeContext>::from_executable(executable)
            .map_err(|err| format!("Assembling executable failed: {err:?}"))
    }
    .unwrap();

    #[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
    verified_executable.jit_compile().unwrap();
    let mut analysis = LazyAnalysis::new(verified_executable.get_executable());

    match matches.value_of("use") {
        Some("cfg") => {
            let mut file = File::create("cfg.dot").unwrap();
            analysis
                .analyze()
                .visualize_graphically(&mut file, None)
                .unwrap();
            return;
        }
        Some("disassembler") => {
            let stdout = std::io::stdout();
            analysis.analyze().disassemble(&mut stdout.lock()).unwrap();
            return;
        }
        _ => {}
    }
    create_vm!(
        vm,
        &verified_executable,
        regions,
        account_lengths,
        &mut invoke_context,
    );
    let mut vm = vm.unwrap();
    let start_time = Instant::now();
    if matches.value_of("use").unwrap() == "debugger" {
        vm.debug_port = Some(matches.value_of("port").unwrap().parse::<u16>().unwrap());
    }
    let (instruction_count, result) = vm.execute_program(interpreted);
    let duration = Instant::now() - start_time;
    if matches.occurrences_of("trace") > 0 {
        // top level trace is stored in syscall_context
        if let Some(Some(syscall_context)) = vm.env.context_object_pointer.syscall_context.last() {
            let trace = syscall_context.trace_log.as_slice();
            output_trace(matches, trace, 0, &mut analysis);
        }
        // the remaining traces are saved in InvokeContext when
        // corresponding syscall_contexts are popped
        let traces = vm.env.context_object_pointer.get_traces();
        for (frame, trace) in traces.iter().filter(|t| !t.is_empty()).enumerate() {
            output_trace(matches, trace, frame + 1, &mut analysis);
        }
    }
    drop(vm);

    let output = Output {
        result: format!("{result:?}"),
        instruction_count,
        execution_time: duration,
        log: invoke_context
            .get_log_collector()
            .unwrap()
            .borrow()
            .get_recorded_content()
            .to_vec(),
    };
    match matches.value_of("output_format") {
        Some("json") => {
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        Some("json-compact") => {
            println!("{}", serde_json::to_string(&output).unwrap());
        }
        _ => {
            println!("Program output:");
            println!("{output:?}");
        }
    }
}
