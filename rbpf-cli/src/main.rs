use {
    clap::{crate_version, Arg, Command},
    serde::{Deserialize, Serialize},
    serde_json::Result,
    solana_bpf_loader_program::{
        create_vm, load_program_from_bytes, serialization::serialize_parameters,
        syscalls::create_loader,
    },
    solana_program_runtime::{
        invoke_context::InvokeContext,
        loaded_programs::{LoadProgramMetrics, LoadedProgramType},
        with_mock_invoke_context,
    },
    solana_rbpf::{
        assembler::assemble, elf::Executable, static_analysis::Analysis,
        verifier::RequisiteVerifier, vm::VerifiedExecutable,
    },
    solana_sdk::{
        account::AccountSharedData,
        bpf_loader,
        pubkey::Pubkey,
        slot_history::Slot,
        transaction_context::{IndexOfAccount, InstructionAccount},
    },
    std::{
        fmt::{Debug, Formatter},
        fs::File,
        io::{Read, Seek, Write},
        path::Path,
        time::{Duration, Instant},
    },
};

#[derive(Serialize, Deserialize, Debug)]
struct Account {
    key: Pubkey,
    owner: Pubkey,
    is_signer: bool,
    is_writable: bool,
    lamports: u64,
    data: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
struct Input {
    accounts: Vec<Account>,
    instruction_data: Vec<u8>,
}
fn load_accounts(path: &Path) -> Result<Input> {
    let file = File::open(path).unwrap();
    let input: Input = serde_json::from_reader(file)?;
    eprintln!("Program input:");
    eprintln!("accounts {:?}", &input.accounts);
    eprintln!("instruction_data {:?}", &input.instruction_data);
    eprintln!("----------------------------------------");
    Ok(input)
}

fn main() {
    solana_logger::setup();
    let matches = Command::new("Solana SBF CLI")
        .version(crate_version!())
        .author("Solana Labs Maintainers <maintainers@solanalabs.com>")
        .about(
            r##"CLI to test and analyze SBF programs.

The tool executes SBF programs in a mocked environment.
Some features, such as sysvars syscall and CPI, are not
available for the programs executed by the CLI tool.

The input data for a program execution have to be in JSON format
and the following fields are required
{
    "accounts": [
        {
            "key": [
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ],
            "owner": [
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ],
            "is_signer": false,
            "is_writable": true,
            "lamports": 1000,
            "data": [0, 0, 0, 3]
        }
    ],
    "instruction_data": []
}
"##,
        )
        .arg(
            Arg::new("PROGRAM")
                .help(
                    "Program file to use. This is either an ELF shared-object file to be executed, \
                     or an assembly file to be assembled and executed.",
                )
                .required(true)
                .index(1)
        )
        .arg(
            Arg::new("input")
                .help(
                    "Input for the program to run on, where FILE is a name of a JSON file \
with input data, or BYTES is the number of 0-valued bytes to allocate for program parameters",
                )
                .short('i')
                .long("input")
                .value_name("FILE / BYTES")
                .takes_value(true)
                .default_value("0"),
        )
        .arg(
            Arg::new("memory")
                .help("Heap memory for the program to run on")
                .short('m')
                .long("memory")
                .value_name("BYTES")
                .takes_value(true)
                .default_value("0"),
        )
        .arg(
            Arg::new("use")
                .help(
                    "Method of execution to use, where 'cfg' generates Control Flow Graph \
of the program, 'disassembler' dumps disassembled code of the program, 'interpreter' runs \
the program in the virtual machine's interpreter, 'debugger' is the same as 'interpreter' \
but hosts a GDB interface, and 'jit' precompiles the program to native machine code \
before execting it in the virtual machine.",
                )
                .short('u')
                .long("use")
                .takes_value(true)
                .value_name("VALUE")
                .possible_values(["cfg", "disassembler", "interpreter", "debugger", "jit"])
                .default_value("jit"),
        )
        .arg(
            Arg::new("instruction limit")
                .help("Limit the number of instructions to execute")
                .short('l')
                .long("limit")
                .takes_value(true)
                .value_name("COUNT")
                .default_value(&std::i64::MAX.to_string()),
        )
        .arg(
            Arg::new("port")
                .help("Port to use for the connection with a remote debugger")
                .long("port")
                .takes_value(true)
                .value_name("PORT")
                .default_value("9001"),
        )
        .arg(
            Arg::new("output_format")
                .help("Return information in specified output format")
                .long("output")
                .value_name("FORMAT")
                .global(true)
                .takes_value(true)
                .possible_values(["json", "json-compact"]),
        )
        .arg(
            Arg::new("trace")
                .help("Output instruction trace")
                .short('t')
                .long("trace")
                .takes_value(true)
                .value_name("FILE"),
        )
        .get_matches();

    let loader_id = bpf_loader::id();
    let mut transaction_accounts = vec![
        (
            loader_id,
            AccountSharedData::new(0, 0, &solana_sdk::native_loader::id()),
        ),
        (
            Pubkey::new_unique(),
            AccountSharedData::new(0, 0, &loader_id),
        ),
    ];
    let mut instruction_accounts = Vec::new();
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
            for (index, account_info) in input.accounts.into_iter().enumerate() {
                let mut account = AccountSharedData::new(
                    account_info.lamports,
                    account_info.data.len(),
                    &account_info.owner,
                );
                account.set_data(account_info.data);
                transaction_accounts.push((account_info.key, account));
                instruction_accounts.push(InstructionAccount {
                    index_in_transaction: index as IndexOfAccount,
                    index_in_caller: index as IndexOfAccount,
                    index_in_callee: index as IndexOfAccount,
                    is_signer: account_info.is_signer,
                    is_writable: account_info.is_writable,
                });
            }
            input.instruction_data
        }
    };
    with_mock_invoke_context!(invoke_context, transaction_context, transaction_accounts);
    invoke_context
        .transaction_context
        .get_next_instruction_context()
        .unwrap()
        .configure(&[0, 1], &instruction_accounts, &instruction_data);
    invoke_context.push().unwrap();
    let (_parameter_bytes, regions, account_lengths) = serialize_parameters(
        invoke_context.transaction_context,
        invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap(),
        true, // should_cap_ix_accounts
    )
    .unwrap();

    let program = matches.value_of("PROGRAM").unwrap();
    let mut file = File::open(Path::new(program)).unwrap();
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).unwrap();
    file.rewind().unwrap();
    let mut contents = Vec::new();
    file.read_to_end(&mut contents).unwrap();
    let mut verified_executable = if magic == [0x7f, 0x45, 0x4c, 0x46] {
        let mut load_program_metrics = LoadProgramMetrics::default();
        let result = load_program_from_bytes(
            &invoke_context.feature_set,
            invoke_context.get_compute_budget(),
            None,
            &mut load_program_metrics,
            &contents,
            &bpf_loader::id(),
            contents.len(),
            Slot::default(),
            false, /* use_jit */
            true,  /* reject_deployment_of_broken_elfs */
            true,  /* debugging_features */
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
    let (instruction_count, result) = vm.execute_program(matches.value_of("use").unwrap() != "jit");
    let duration = Instant::now() - start_time;
    if matches.occurrences_of("trace") > 0 {
        for (frame, syscall_context) in vm
            .env
            .context_object_pointer
            .syscall_context
            .iter()
            .enumerate()
        {
            if syscall_context.is_none() {
                continue;
            }
            let trace_log = syscall_context.as_ref().unwrap().trace_log.as_slice();
            if matches.value_of("trace").unwrap() == "stdout" {
                writeln!(&mut std::io::stdout(), "Frame {frame}").unwrap();
                analysis
                    .analyze()
                    .disassemble_trace_log(&mut std::io::stdout(), trace_log)
                    .unwrap();
            } else {
                let filename = format!("{}.{}", matches.value_of("trace").unwrap(), frame);
                let mut fd = File::create(filename).unwrap();
                writeln!(&fd, "Frame {frame}").unwrap();
                analysis
                    .analyze()
                    .disassemble_trace_log(&mut fd, trace_log)
                    .unwrap();
            }
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
