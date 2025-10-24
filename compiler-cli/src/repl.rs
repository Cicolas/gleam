use camino::Utf8Path;
use gleam_core::{
    Error,
    analyse::TargetSupport,
    ast::{
        Definition, Function, Pattern, Statement, TargetedDefinition, TypedDefinition,
        TypedFunction, UntypedDefinition, UntypedExpr, UntypedStatement,
    },
    build::{Built, Codegen, Compile, Mode, Module, Options, Runtime, Target},
    io::FileSystemWriter,
    parse::{self, ReplItem},
    paths::ProjectPaths,
    type_::{ModuleInterface, Type, printer::Printer},
    warning::NullWarningEmitterIO,
};

use rustyline::{DefaultEditor, error::ReadlineError};
use tempfile::{self, TempPath};

use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::Write as Writefmt,
    io::{self, Stdout, Write, stdout},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    rc::Rc,
    sync::OnceLock,
};

use crate::{
    cli,
    fs::{ConsoleWarningEmitter, ProjectIO},
    repl::expect::Session,
};

#[macro_export]
macro_rules! swrite {
    ($s:expr, $($arg:tt)*) => {
        let _ = write!($s, $($arg)*);
    };
}

macro_rules! swriteln {
    ($s:expr, $($arg:tt)*) => {
        let _ = writeln!($s, $($arg)*);
    };
}

const PROMPT: &str = "$ ";
const HISTORY_FILE: &str = ".gleam_history";
const QUIT: &str = ":quit";
const TYPE: &str = ":type ";

// FIXME: use echo template file
const GLEAM_REPL_MJS: &[u8] = include_bytes!("gleam_repl.mjs");
const GLEAM_REPL_ERL: &[u8] = include_bytes!("gleam_repl.erl");

const REPL_MAIN: &str = "repl_main";
const REPL_JS_FNS: &str = r#"
@external(javascript, "../gleam_repl.mjs", "repl_save")
pub fn repl_save(value: a) -> a

@external(javascript, "../gleam_repl.mjs", "repl_load")
pub fn repl_load(index: Int) -> a

@external(javascript, "../gleam_repl.mjs", "repl_print")
pub fn repl_print(value: a) -> a
"#;
const REPL_ERL_FNS: &str = r#"
@external(erlang, "gleam_repl", "repl_save")
pub fn repl_save(value: a) -> a

@external(erlang, "gleam_repl", "repl_load")
pub fn repl_load(index: Int) -> a

@external(erlang, "gleam_repl", "repl_has_var")
pub fn repl_has_var(index: Int) -> a

@external(erlang, "gleam_repl", "repl_print")
pub fn repl_print(value: a) -> a
"#;
const GLEAM_BREAK_CODE: &str = "GlEaM";

#[derive(Clone)]
pub enum ReplRuntime {
    Erlang,
    JavaScript(Runtime),
}

pub fn command(
    paths: &ProjectPaths,
    target: Option<Target>,
    runtime: Option<Runtime>,
    module: Option<String>,
) -> Result<(), Error> {
    let mut repl = setup(paths, target, runtime, module)?;

    let mut editor = DefaultEditor::new().unwrap();
    if let Some(history) = &history_path() {
        let _ = editor.load_history(history);
    }

    println!("Type ctrl-d ou \"{QUIT}\" to exit.");
    loop {
        match editor.readline(PROMPT) {
            Ok(input) => {
                let input_trim = input.trim();
                if input_trim.is_empty() || input_trim == QUIT {
                    continue;
                }
                let _ = editor.add_history_entry(&input);

                if let Err(error) = repl.run(&input) {
                    let stderr = cli::stderr_buffer_writer();
                    let mut buffer = stderr.buffer();
                    error.pretty(&mut buffer);
                    stderr.print(&buffer).expect("Final result error writing");
                }
            }
            Err(ReadlineError::Interrupted) => {
                break;
            }
            Err(err) => {
                if !matches!(err, ReadlineError::Eof) {
                    // FIXME: improve error message
                    println!("Error: {:?}", err);
                }
                if let Some(history) = &history_path() {
                    let _ = editor.save_history(history);
                }
                break;
            }
        }
    }

    Ok(())
}

fn setup(
    paths: &ProjectPaths,
    target: Option<Target>,
    runtime: Option<Runtime>,
    module: Option<String>,
) -> Result<Repl, Error> {
    // Validate the module path
    if let Some(mod_path) = &module {
        if !is_gleam_module(mod_path) {
            return Err(Error::InvalidModuleName {
                module: mod_path.to_owned(),
            });
        }
    };

    let built = build_with_progress(paths)?;
    let mod_config = &built.root_package.config;
    let package = &built.root_package.config.name;
    let module = built.module_interfaces.get(package);

    let target = target.unwrap_or(mod_config.target);

    match target {
        Target::Erlang => match runtime {
            Some(r) => Err(Error::InvalidRuntime {
                target: Target::Erlang,
                invalid_runtime: r,
            }),
            _ => Ok(Repl::new(paths.clone(), package.into(), module, ReplRuntime::Erlang).unwrap()),
        },
        Target::JavaScript => match runtime.unwrap_or(mod_config.javascript.runtime) {
            Runtime::NodeJs => todo!(),
            Runtime::Deno => todo!(),
            Runtime::Bun => todo!(),
        },
    }
}

/// Check if a module name is a valid gleam module name.
fn is_gleam_module(module: &str) -> bool {
    use regex::Regex;
    static RE: OnceLock<Regex> = OnceLock::new();

    RE.get_or_init(|| {
        Regex::new(&format!(
            "^({module}{slash})*{module}$",
            module = "[a-z][_a-z0-9]*",
            slash = "/",
        ))
        .expect("is_gleam_module() RE regex")
    })
    .is_match(module)
}

fn build_with_progress(paths: &ProjectPaths) -> Result<Built, Error> {
    build(paths, true, true)
}

fn build_without_progress(paths: &ProjectPaths) -> Result<Built, Error> {
    build(paths, false, false)
}

fn build(paths: &ProjectPaths, progress: bool, warnings: bool) -> Result<Built, Error> {
    crate::build::main_with_warnings(
        paths,
        Options {
            root_target_support: TargetSupport::Enforced,
            warnings_as_errors: false,
            codegen: Codegen::All,
            compile: Compile::All,
            mode: Mode::Dev,
            target: Some(Target::Erlang),
            no_print_progress: !progress,
        },
        crate::build::download_dependencies(paths, cli::Reporter::new())?,
        if warnings {
            Rc::new(ConsoleWarningEmitter)
        } else {
            Rc::new(NullWarningEmitterIO)
        },
    )
}

trait Engine {
    fn run_main(&mut self, module: &str);

    fn has_var(&mut self, index: usize) -> bool;
}

#[derive(Clone)]
struct Erlang {
    session: Rc<RefCell<ReplSession>>,
    package: String,
    command_num: usize,
}

impl Erlang {
    fn new(_paths: ProjectPaths, package: String) -> Self {
        let mut erl_shell = Command::new("erl");

        let mut session =
            ReplSession::new(&mut erl_shell).expect("Unable to create erl ReplSession");
        session.pipe_output(false);

        // Wait for first prompt
        let _ = session.expect(format!("{}> ", 1).as_str());

        let mut erl = Erlang {
            session: Rc::new(RefCell::new(session)),
            package,
            command_num: 1,
        };

        let package_name = erl.package.clone();

        // TODO: load project packages
        // Load gleam stdlib into erl shell
        erl.import_package("gleam_stdlib");

        // Load the package binaries into erl shell
        erl.import_package(&package_name);

        erl
    }

    fn write_import(&mut self, code: &str) -> io::Result<()> {
        let mut session = self.session.borrow_mut();
        session.pipe_output(false);

        let _ = session.write(code.trim().as_bytes())?;
        self.command_num += 1;

        let _ = session.expect(format!("true\n").as_str())?;
        let _ = session.expect_prompt(self.command_num);

        Ok(())
    }

    fn write_code(&mut self, code: &str) -> io::Result<()> {
        let mut session = self.session.borrow_mut();
        session.pipe_output(true);

        let _ = session.write(code.trim().as_bytes())?;
        self.command_num += 1;

        let _ = session.expect(format!("'{GLEAM_BREAK_CODE}'").as_str())?;
        let _ = session.expect_prompt(self.command_num);

        Ok(())
    }

    fn import_package(&mut self, package_name: &str) {
        self.write_import(
            format!(
                "code:add_patha(\"build/dev/erlang/{}/ebin/\").",
                package_name
            )
            .as_str(),
        )
        .expect(format!("Unable to run {} import", package_name).as_str());
    }
}

impl Engine for Erlang {
    fn run_main(&mut self, module: &str) {
        let mut code = format!(
            "try
                {module}:repl_main(),
                '{GLEAM_BREAK_CODE}'
            catch error:#{{
                    gleam_error := GleamError,
                    module := Module,
                    function := Function,
                    line := Line
                }} ->
                    io:format(
                        standard_error,
                        \"Error at ~s.~s:~p\\n  Gleam error: ~p\\n\",
                        [Module, Function, Line, GleamError]
                    ), '{GLEAM_BREAK_CODE}';

                Class:Reason ->
                    io:format(standard_error, \"Internal Erlang Error: ~p~n\", [{{Class, Reason}}]),
                    '{GLEAM_BREAK_CODE}'
            end."
        );
        code = code
            .lines()
            .map(|line| {
                let trimmed_line = line.trim();
                format!("{trimmed_line} ")
            })
            .collect::<String>();

        self.write_code(code.trim())
            .expect("Unable to run the code");
    }

    fn has_var(&mut self, index: usize) -> bool {
        let code = format!("gleam_repl:repl_has_var({index}).");
        let mut session = self.session.borrow_mut();

        let _ = session.write(code.trim().as_bytes()).unwrap();
        self.command_num += 1;

        let found_var = session.expect("true\n").expect("Unable to expect has_var");
        let _ = session.expect_prompt(self.command_num);
        found_var
    }
}

#[derive(Clone)]
struct Repl {
    user_import: Option<String>,
    imports: Vec<String>,
    consts: Vec<String>,
    types: Vec<String>,
    fns: HashMap<String, String>,
    vars: HashMap<String, Value>,
    paths: ProjectPaths,
    project: ProjectIO,
    engine: Rc<RefCell<dyn Engine>>,
    runtime: ReplRuntime,
    iter: (usize, usize),
    var_index: usize,
}

#[derive(Clone)]
struct Value {
    index: usize,
    type_: String,
}

impl Repl {
    pub fn new(
        paths: ProjectPaths,
        package: String,
        module: Option<&ModuleInterface>,
        runtime: ReplRuntime
    ) -> Result<Self, Error> {
        let project = ProjectIO::new();
        let path = TempPath::from_path(paths.src_directory().join("gleam_repl.erl"));

        project
            .write_bytes(Utf8Path::from_path(&path).unwrap(), GLEAM_REPL_ERL)
            .unwrap();

        let _ = build_without_progress(&paths)?;

        let engine = match runtime {
            ReplRuntime::Erlang => {
                Erlang::new(paths.clone(), package)
            },
            ReplRuntime::JavaScript(Runtime::Deno) => {
                todo!()
            },
            ReplRuntime::JavaScript(Runtime::NodeJs) => {
                todo!()
            },
            ReplRuntime::JavaScript(Runtime::Bun) => {
                todo!()
            }
        };

        Ok(Repl {
            user_import: module.map(import_public_types_and_values),
            imports: vec![],
            consts: vec![],
            types: vec![],
            fns: HashMap::new(),
            vars: HashMap::new(),
            paths: paths.clone(),
            project: project.clone(),
            engine: Rc::new(RefCell::new(engine)),
            runtime: runtime,
            iter: (0, 0),
            var_index: 0,
        })
    }

    pub fn run(&mut self, mut input: &str) -> Result<(), Error> {
        self.iter = (self.iter.0 + 1, 0);
        let line_trim = input.trim();

        let type_ = if let Some(expr) = line_trim.strip_prefix(TYPE) {
            input = expr;
            true
        } else {
            false
        };

        let items = parse::parse_repl(input).map_err(|error| Error::Parse {
            path: "repl".into(),
            src: input.into(),
            error: error.into(),
        })?;

        if type_ && items.len() != 1 {
            println!("{TYPE}command expects exactly one expression.");
            return Ok(());
        }

        // FIXME: avoid this clone
        // We clone self so we can rollback if the execution fail
        let repl = (*self).clone();

        for item in items {
            self.iter.1 += 1;
            let result = match item {
                ReplItem::ReplDefinition(_) if type_ => {
                    println!("{TYPE}command cannot be used with definitions.");
                    continue;
                }
                ReplItem::ReplDefinition(t) => self.run_definition(t, input),
                ReplItem::ReplStatement(_) if type_ => self.run_type_cmd(input),
                ReplItem::ReplStatement(s) => self.run_statement(s, input),
            };

            if let Err(err) = result {
                *self = repl;
                return Err(err);
            }
        }

        Ok(())
    }

    fn build_source(&self) -> String {
        let mut src = String::new();
        src.push_str(REPL_ERL_FNS);
        self.add_imports(&mut src);
        self.add_consts(&mut src);
        self.add_types(&mut src);
        self.add_fns(&mut src);
        src
    }

    fn compile(&mut self, code: &str) -> Result<Vec<Module>, Error> {
        // FIXME: avoid name collision
        let path = TempPath::from_path(
            self.paths
                .src_directory()
                .join(format!("repl{}_{}.gleam", self.iter.0, self.iter.1)),
        );

        let module_name = path.file_stem().unwrap().to_str().unwrap();

        // TODO: add an option to show the generated code?
        self.project
            .write(Utf8Path::from_path(&path).unwrap(), code)
            .unwrap();

        let mut modules = build_without_progress(&self.paths)?.root_package.modules;

        let pos = modules
            .iter()
            .position(|module| module.name == module_name)
            .expect("The repl module");

        // FIXME: use Vec1
        let mut modules1 = vec![modules.swap_remove(pos)];
        modules1.extend(modules);

        Ok(modules1)
    }

    fn run_definition(&mut self, targeted: TargetedDefinition, src: &str) -> Result<(), Error> {
        let mut src = get_definition_src(&targeted.definition, src).into();

        match &targeted.definition {
            Definition::Import(_) => self.run_import(src),
            Definition::TypeAlias(_) | Definition::CustomType(_) => self.run_type(src),
            Definition::ModuleConstant(_) => self.run_const(src),
            Definition::Function(f) => {
                let lets = self.gen_lets(&get_args_names(f));

                src.insert_str(
                    (f.body.first().location().start - targeted.definition.location().start)
                        as usize,
                    &format!("\n  {lets}"),
                );

                let name = f.name.clone().expect("A function must have a name").1;
                self.run_fn(name.into(), src)
            }
        }
    }

    fn run_statement(&mut self, statement: UntypedStatement, src: &str) -> Result<(), Error> {
        let start = statement.location().start as usize;
        let end = statement.location().end as usize;

        match statement {
            Statement::Use(_) => self.run_use(&src[start..end]),
            Statement::Expression(_) => self.run_expr(&src[start..end]),
            Statement::Assignment(a) => match a.pattern {
                Pattern::Variable { name, .. } => {
                    let end = a.value.location().end as usize;
                    self.run_let(name.as_str(), &src[start..end])
                }
                Pattern::Discard { .. } => {
                    let end = a.value.location().end as usize;
                    self.run_expr(&src[start..end])
                }
                _ => {
                    println!("patterns are not supported in let statements.");
                    Ok(())
                }
            },
            Statement::Assert(_) => {
                println!("assert is not supported.");
                Ok(())
            }
        }
    }

    fn run_type_cmd(&mut self, code: &str) -> Result<(), Error> {
        let mut src = self.build_source();
        self.add_expr(&mut src, code);
        let module = self.compile(&src)?.into_iter().next().unwrap();
        let main = &get_function(&module, REPL_MAIN).expect("repl main function");
        println!("{}", type_to_string(&module, &main.return_type));
        Ok(())
    }

    fn run_check(&mut self) -> Result<(), Error> {
        self.compile(&self.build_source()).map(|_| ())
    }

    fn run_let(&mut self, name: &str, code: &str) -> Result<(), Error> {
        let mut src = self.build_source();
        let lets = self.gen_lets(&[]);
        // FIXME: avoid name collision
        src.push_str(&format! {"
            pub fn {REPL_MAIN}() {{
              run_save()
              Nil
            }}

            pub fn run_save() {{
              {lets}
              repl_print(repl_save({{
            {code}
              }}))
            }}
            "
        });

        let module = self.compile(&src)?.into_iter().next().unwrap();

        self.engine.borrow_mut().run_main(&module.name);

        if self.engine.borrow_mut().has_var(self.var_index) {
            let main = get_function(&module, "run_save").expect("repl main function");
            let type_ = type_to_string(&module, &main.return_type);
            let index = self.var_index;
            let _ = self.vars.insert(name.into(), Value { index, type_ });
            self.var_index += 1;
        } else {
            // there was an error and the variable was not saved
        }

        Ok(())
    }

    fn run_expr(&mut self, code: &str) -> Result<(), Error> {
        let mut src = self.build_source();
        self.add_expr(&mut src, code);
        let module = self.compile(&src)?.into_iter().next().unwrap();
        self.engine.borrow_mut().run_main(&module.name);
        Ok(())
    }

    fn run_import(&mut self, _code: String) -> Result<(), Error> {
        println!("imports are not supported.");
        Ok(())
        // TODO: implement import merge
        // import gleam/string.{append}
        // import gleam/string.{inspect}
        // -> import gleam/string.{append, inspect}
    }

    fn run_const(&mut self, code: String) -> Result<(), Error> {
        // TODO: improve error message for const redefinition
        self.consts.push(code);
        self.run_check()
    }

    fn run_type(&mut self, code: String) -> Result<(), Error> {
        // TODO: improve error message for type redefinition
        self.types.push(code);
        self.run_check()
    }

    fn run_fn(&mut self, name: String, code: String) -> Result<(), Error> {
        let _ = self.fns.insert(name, code);
        self.run_check()
    }

    fn run_use(&mut self, _code: &str) -> Result<(), Error> {
        println!("use statements are not supported outside blocks.");
        Ok(())
    }

    fn add_expr(&self, src: &mut String, expr: &str) {
        let lets = self.gen_lets(&[]);
        src.push_str(&format! {"
            pub fn {REPL_MAIN}() {{
              {lets}
              repl_print({{
            {expr}
              }})
            }}
            "
        });
    }

    fn add_imports(&self, src: &mut String) {
        if let Some(user) = &self.user_import {
            swriteln!(src, "{user}");
        }
        for import in &self.imports {
            swriteln!(src, "import {import}");
        }
    }

    fn add_consts(&self, src: &mut String) {
        for const_ in &self.consts {
            swriteln!(src, "{const_}");
        }
    }

    fn add_types(&self, src: &mut String) {
        for type_ in &self.types {
            swriteln!(src, "{type_}");
        }
    }

    fn add_fns(&self, src: &mut String) {
        for code in self.fns.values() {
            swriteln!(src, "{code}");
        }
    }

    fn gen_lets(&self, exclude: &[String]) -> String {
        let mut lets = String::new();
        for (name, Value { index, type_ }) in &self.vars {
            if !exclude.contains(name) {
                swriteln!(
                    lets,
                    r#"  let {name} = fn () -> {type_} {{ repl_load({index}) }} ()"#
                );
            }
        }
        lets
    }
}

struct ReplSession {
    child_stdin: ChildStdin,
    session: Session<ChildStdout, Stdout>,
    child: Child,
}

impl ReplSession {
    fn new(command: &mut Command) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();

        let session = Session::new(child_stdout, stdout());

        Ok(Self {
            child_stdin,
            session,
            child,
        })
    }

    fn expect(&mut self, token: &str) -> io::Result<bool> {
        self.session.expect(token)
    }

    fn expect_prompt(&mut self, command_num: usize) -> io::Result<bool> {
        self.session.expect(format!("{}> ", command_num).as_str())
    }

    fn pipe_output(&mut self, enable: bool) {
        self.session.pipe_output(enable)
    }
}

impl Write for ReplSession {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.child_stdin.write(buf)?;
        self.child_stdin.write(b"\n").map(|u| u + written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.child_stdin.flush()
    }
}

fn get_function<'a>(module: &'a Module, name: &str) -> Option<&'a TypedFunction> {
    module.ast.definitions.iter().find_map(|def| match def {
        TypedDefinition::Function(f) if f.name.as_ref().map(|s| s.1.as_str()) == Some(name) => {
            Some(f)
        }
        _ => None,
    })
}

fn get_definition_src<'a>(def: &UntypedDefinition, src: &'a str) -> &'a str {
    let start = def.location().start as usize;
    let end = def.location().end as usize;
    let end = match def {
        Definition::TypeAlias(_) | Definition::Import(_) => end,
        Definition::CustomType(type_) => type_.end_position as usize,
        Definition::ModuleConstant(const_) => const_.value.location().end as usize,
        Definition::Function(f) => f.end_position as usize,
    };

    &src[start..end]
}

fn get_args_names(fun: &Function<(), UntypedExpr>) -> Vec<String> {
    fun.arguments
        .iter()
        .filter_map(|arg| arg.names.get_variable_name().map(String::from))
        .collect()
}

fn type_to_string(module: &Module, type_: &Type) -> String {
    Printer::new(&module.ast.names).print_type(type_).into()
}

fn import_public_types_and_values(module: &ModuleInterface) -> String {
    let mut import = String::new();
    let name = &module.name;
    swrite!(&mut import, "import {name}.{{");
    for type_ in module.public_type_names() {
        swrite!(&mut import, "type {type_}, ");
    }
    for value in module.public_value_names() {
        swrite!(&mut import, "{value}, ");
    }
    import.push('}');
    import
}

fn history_path() -> Option<PathBuf> {
    dirs::home_dir().map(|p| p.join(HISTORY_FILE))
}

mod expect {
    use std::{
        fs::File,
        io::{self, Error, Read, Write},
        ops::Deref,
    };

    pub struct Session<R, W> {
        input: R,
        output: W,
        buffer: Buffer,
        file: File,
        pipe_enabled: bool,
    }

    impl<R: Read, W: Write> Session<R, W> {
        pub fn new(input: R, output: W) -> Self {
            Session {
                input,
                output,
                buffer: Buffer::new(),
                file: File::create("log").unwrap(),
                pipe_enabled: true,
            }
        }

        /// Produces Ok(true) token is found.
        /// Produces Ok(false) if the input is over and the token is not found.
        /// Return Error if reading the input fails.
        pub fn expect(&mut self, token: &str) -> Result<bool, Error> {
            let token = token.as_bytes();
            let token_len = token.len();
            loop {
                if let Some(pos) = self.buffer.windows(token_len).position(|w| w == token) {
                    self.write_and_consume(pos, pos + token_len)?;
                    return Ok(true);
                }

                // token not found, leave at most token - 1 bytes on the buffer.
                let to_write = self.buffer.len().saturating_sub(token_len - 1);
                self.write_and_consume(to_write, to_write)?;

                let n = self.buffer.read(&mut self.input, &mut self.file)?;
                if n == 0 {
                    // the input is over, write the remaning bytes.
                    let len = self.buffer.len();
                    self.write_and_consume(len, len)?;
                    return Ok(false);
                }
            }
        }

        fn write_and_consume(&mut self, len: usize, consume: usize) -> Result<(), Error> {
            if self.pipe_enabled {
                let _ = self.output.write(&self.buffer[..len])?;
            }
            self.output.flush()?;
            self.buffer.consume(consume);
            Ok(())
        }

        pub fn pipe_output(&mut self, enable: bool) {
            self.pipe_enabled = enable;
        }
    }

    struct Buffer {
        buffer: [u8; 1024],
        used: usize,
    }

    impl Buffer {
        fn new() -> Self {
            Buffer {
                buffer: [0; 1024],
                used: 0,
            }
        }

        fn read<R: Read>(&mut self, mut reader: R, file: &mut File) -> io::Result<usize> {
            let _ = write!(file, "a: ");
            let _ = file.write(&self.buffer[..self.used]).unwrap();
            let _ = write!(file, "\n");
            let n = reader.read(&mut self.buffer[self.used..])?;
            let _ = write!(file, "b: ");
            let _ = file.write(&self.buffer[self.used..self.used + n]).unwrap();
            let _ = write!(file, "\n");
            file.flush().unwrap();
            let _ = writeln!(file, "");
            self.used += n;
            Ok(n)
        }

        fn consume(&mut self, n: usize) {
            self.buffer.copy_within(n.., 0);
            self.used -= n;
        }
    }

    impl Deref for Buffer {
        type Target = [u8];

        fn deref(&self) -> &Self::Target {
            &self.buffer[..self.used]
        }
    }

    #[test]
    fn test() {
        use std::thread;
        use std::time::Duration;
        let (input, mut pipe) = io::pipe().unwrap();
        let _ = thread::spawn(move || {
            write!(&mut pipe, "Some STOPwordsSTO").unwrap();
            pipe.flush().unwrap();
            thread::sleep(Duration::from_millis(100));
            writeln!(&mut pipe, "Pother STOPend").unwrap();
        });
        let mut output = Vec::<u8>::new();
        let mut session = Session::new(input, &mut output);
        while session.expect("STOP").unwrap() {}
        assert_eq!("Some wordsother end\n", String::from_utf8(output).unwrap());
    }
}
