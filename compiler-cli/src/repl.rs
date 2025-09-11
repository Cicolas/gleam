use camino::Utf8Path;
use gleam_core::{
    Error,
    analyse::TargetSupport,
    ast::{
        Definition, Function, Pattern, Statement, TargetedDefinition, TypedDefinition,
        TypedFunction, UntypedDefinition, UntypedExpr, UntypedStatement,
    },
    build::{Built, Codegen, Compile, Mode, Module, Options, Target},
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
    fmt::Write,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{ChildStdin, ChildStdout, Command, Stdio},
    rc::Rc, thread,
};

use crate::{
    cli,
    fs::{ConsoleWarningEmitter, ProjectIO},
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

const PROMPT: &str = "> ";
const HISTORY_FILE: &str = ".gleam_history";
const QUIT: &str = ":quit";
const TYPE: &str = ":type ";

// FIXME: use echo template file
const GLEAM_REPL_MJS: &[u8] = include_bytes!("gleam_repl.mjs");

const REPL_MAIN: &str = "repl_main";
const REPL_FNS: &str = r#"
@external(javascript, "../gleam_repl.mjs", "repl_save")
pub fn repl_save(value: a) -> a

@external(javascript, "../gleam_repl.mjs", "repl_load")
pub fn repl_load(index: Int) -> a

@external(javascript, "../gleam_repl.mjs", "repl_print")
pub fn repl_print(value: a) -> a
"#;

pub fn command(paths: &ProjectPaths) -> Result<(), Error> {
    let built = build_with_progress(paths)?;
    let package = &built.root_package.config.name;
    let module = built.module_interfaces.get(package);

    let mut repl = Repl::<Deno>::new(paths.clone(), package.into(), module).unwrap();

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
            target: Some(Target::JavaScript),
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

trait Engine: Clone {
    fn new(paths: ProjectPaths, package: String) -> Self;

    fn run_main(&mut self, module: &str);

    fn has_var(&self, index: usize) -> bool;
}

#[derive(Clone)]
struct Deno {
    stdin: Rc<RefCell<ChildStdin>>,
    stdout: Rc<RefCell<ChildStdout>>,
    paths: ProjectPaths,
    package: String,
}

impl Engine for Deno {
    fn new(paths: ProjectPaths, package: String) -> Self {
        let command = Command::new("deno")
            .arg("repl")
            .arg("--allow-read")
            .arg("--quiet")
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Could not spawn deno subprocess");

        let stdin = command.stdin.expect("Could not take Deno stdin");
        let stdout = command.stdout.expect("Could not take Deno stdout");
        let mut stderr = command.stderr.expect("Could not take Deno stderr");

        let _ = thread::spawn(move || {
            let mut buffer = [0; 1024];
            loop {
                let n = stderr.read(&mut buffer).unwrap();
                if n > 0 {
                    print!("{}", String::from_utf8_lossy(&buffer[..n]));
                    std::io::Write::flush(&mut std::io::stdout()).unwrap();
                }
            }
        });

        Deno {
            stdin: Rc::new(RefCell::new(stdin)),
            stdout: Rc::new(RefCell::new(stdout)),
            paths,
            package,
        }
    }

    fn run_main(&mut self, module: &str) {
        let path = self
            .paths
            .build_directory_for_target(Mode::Dev, Target::JavaScript)
            .join(&self.package)
            .join(format!("{module}.mjs"));

        let mut stdin_ref = self.stdin.borrow_mut();
        let _ = std::io::Write::write_all(
            &mut *stdin_ref,
            format!("import {{ {REPL_MAIN} }} from \"{path}\"; {REPL_MAIN}();\n").as_bytes(),
        );

        let mut stdout_ref = self.stdout.borrow_mut();
        let mut reader = BufReader::new(&mut *stdout_ref);
        let mut buffer = String::new();
        loop {
            buffer.clear();
            let bytes_read = reader.read_line(&mut buffer).expect("Unable to read chunk");
            let trimmed = buffer.trim();

            if bytes_read == 0 || trimmed == "undefined" {
                break;
            }
            println!("{trimmed}");
        }
    }

    fn has_var(&self, index: usize) -> bool {
        let mut stdin_ref = self.stdin.borrow_mut();
        let _ = std::io::Write::write_all(
            &mut *stdin_ref,
            format!("{index} < globalThis.repl_vars.length\n").as_bytes(),
        );

        let mut stdout_ref = self.stdout.borrow_mut();
        let mut reader = BufReader::new(&mut *stdout_ref);
        let mut buffer = String::new();
        loop {
            buffer.clear();
            let _ = reader.read_line(&mut buffer).expect("Unable to read chunk");
            let trimmed = buffer.trim();

            match trimmed {
                "true" => return true,
                "false" => return false,
                _ => {}
            }
        }
    }
}

#[derive(Clone)]
struct Repl<E: Engine> {
    user_import: Option<String>,
    imports: Vec<String>,
    consts: Vec<String>,
    types: Vec<String>,
    fns: HashMap<String, String>,
    vars: HashMap<String, Value>,
    paths: ProjectPaths,
    project: ProjectIO,
    engine: E,
    iter: (usize, usize),
    var_index: usize,
}

#[derive(Clone)]
struct Value {
    index: usize,
    type_: String,
}

impl<E: Engine> Repl<E> {
    pub fn new(
        paths: ProjectPaths,
        package: String,
        module: Option<&ModuleInterface>,
    ) -> Result<Repl<E>, Error> {
        let project = ProjectIO::new();
        let build_dir = paths.build_directory_for_target(Mode::Dev, Target::JavaScript);
        project
            .write_bytes(&build_dir.join("gleam_repl.mjs"), GLEAM_REPL_MJS)
            .unwrap();
        Ok(Repl {
            user_import: module.map(import_public_types_and_values),
            imports: vec![],
            consts: vec![],
            types: vec![],
            fns: HashMap::new(),
            vars: HashMap::new(),
            paths: paths.clone(),
            project: project.clone(),
            engine: E::new(paths, package),
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
        src.push_str(REPL_FNS);
        self.add_imports(&mut src);
        self.add_consts(&mut src);
        self.add_types(&mut src);
        self.add_fns(&mut src);
        src
    }

    fn compile(&mut self, code: &str) -> Result<Vec<Module>, Error> {
        // FIXME: avoid name collision
        let path = TempPath::from_path(self.paths.src_directory().join(format!("repl{}_{}.gleam", self.iter.0, self.iter.1)));

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
              {lets}
              repl_print(repl_save({{
            {code}
              }}))
              Nil
            }}
            "
        });

        let module = self.compile(&src)?.into_iter().next().unwrap();

        self.engine.run_main(&module.name);

        if self.engine.has_var(self.var_index) {
            let main = get_function(&module, REPL_MAIN).expect("repl main function");
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
        self.engine.run_main(&module.name);
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
              Nil
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
