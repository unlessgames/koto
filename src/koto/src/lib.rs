pub use koto_bytecode::{bytecode_to_string, Compiler};
pub use koto_parser::{
    vec4::Vec4, AstNode, Function, KotoParser as Parser, LookupOrId, LookupSliceOrId, Position,
};
use koto_runtime::Vm;
pub use koto_runtime::{
    external_error, make_external_value, type_as_string, Error, ExternalValue, RuntimeResult,
    Value, ValueHashMap, ValueList, ValueMap, ValueVec, VmRuntimeFunction,
};
pub use koto_std::{get_external_instance, visit_external_value};
use std::{path::Path, sync::Arc};

#[derive(Default)]
pub struct Koto {
    script: String,
    parser: Parser,
    compiler: Compiler,
    ast: AstNode,
    runtime: Vm,
}

impl Koto {
    pub fn new() -> Self {
        let mut result = Self::default();

        koto_std::register(&mut result.runtime);

        let mut env = ValueMap::new();
        env.add_value("script_dir", Value::Empty);
        env.add_value("script_path", Value::Empty);
        env.add_list("args", ValueList::new());
        result.runtime.global_mut().add_map("env", env);

        result
    }

    pub fn run_script(&mut self, script: &str) -> Result<Value, String> {
        self.compile(script)?;

        self.set_args(Vec::new());
        self.run()?;

        if let Some(main) = self.get_global_function("main") {
            self.call_function(&main, &[])
        } else {
            Ok(Value::Empty)
        }
    }

    pub fn run_script_with_args(
        &mut self,
        script: &str,
        args: Vec<String>,
    ) -> Result<Value, String> {
        self.compile(script)?;

        self.set_args(args);
        self.run()?;

        if let Some(main) = self.get_global_function("main") {
            self.call_function(&main, &[])
        } else {
            Ok(Value::Empty)
        }
    }

    pub fn compile(&mut self, script: &str) -> Result<(), String> {
        match self.parser.parse(&script, self.runtime.constants_mut()) {
            Ok(ast) => {
                self.ast = ast;
                self.runtime.constants_mut().shrink_to_fit();
            }
            Err(e) => {
                return Err(format!("Error while parsing script: {}", e));
            }
        }
        match self.compiler.compile_ast(&self.ast) {
            Ok(bytecode) => {
                self.runtime.set_bytecode(bytecode);

                // TODO make optional
                // println!("{}", script);
                // println!("{}", bytecode_to_string(bytecode));

                self.script = script.to_string();
                Ok(())
            }
            Err(e) => Err(format!("Error while compiling script: {}", e)),
        }
    }

    pub fn global_mut(&mut self) -> &mut ValueMap {
        self.runtime.global_mut()
    }

    pub fn set_args(&mut self, args: Vec<String>) {
        use Value::{Map, Str};

        let koto_args = args
            .iter()
            .map(|arg| Str(Arc::new(arg.to_string())))
            .collect::<ValueVec>();

        match self.runtime.global_mut().data_mut().get_mut("env").unwrap() {
            Map(map) => map
                .data_mut()
                .add_list("args", ValueList::with_data(koto_args)),
            _ => unreachable!(),
        }
    }

    pub fn set_script_path(&mut self, path: Option<String>) {
        use Value::{Empty, Map, Str};

        let (script_dir, script_path) = match &path {
            Some(path) => (
                Path::new(&path)
                    .parent()
                    .map(|p| {
                        Str(Arc::new(
                            p.to_str().expect("invalid script path").to_string(),
                        ))
                    })
                    .or(Some(Empty))
                    .unwrap(),
                Str(Arc::new(path.to_string())),
            ),
            None => (Empty, Empty),
        };

        self.runtime.set_script_path(path);

        match self.runtime.global_mut().data_mut().get_mut("env").unwrap() {
            Map(map) => {
                let mut map = map.data_mut();
                map.add_value("script_dir", script_dir);
                map.add_value("script_path", script_path);
            }
            _ => unreachable!(),
        }
    }

    pub fn run(&mut self) -> Result<Value, String> {
        match self.runtime.run() {
            Ok(result) => Ok(result),
            Err(e) => Err(match &e {
                Error::RuntimeError {
                    message,
                    start_pos,
                    end_pos,
                } => self.format_runtime_error(message, start_pos, end_pos),
                Error::VmRuntimeError {
                    message,
                    instruction,
                } => {
                    format!(
                        "VM Runtime error at instruction {}: {}\n",
                        instruction, message
                    ) // TODO
                }
                Error::ExternalError { message } => format!("External error: {}\n", message),
            }),
        }
    }

    pub fn get_global_function(&self, id: &str) -> Option<VmRuntimeFunction> {
        match self.runtime.get_global_value(id) {
            Some(Value::VmFunction(function)) => Some(function),
            _ => None,
        }
    }

    pub fn call_function_by_name(
        &mut self,
        function_name: &str,
        args: &[Value],
    ) -> Result<Value, String> {
        match self.get_global_function(function_name) {
            Some(f) => self.call_function(&f, args),
            None => Err(format!(
                "Runtime error: function '{}' not found",
                function_name
            )),
        }
    }

    pub fn call_function(
        &mut self,
        function: &VmRuntimeFunction,
        args: &[Value],
    ) -> Result<Value, String> {
        match self.runtime.run_function(function, args) {
            Ok(result) => Ok(result),
            Err(e) => Err(match &e {
                Error::RuntimeError {
                    message,
                    start_pos,
                    end_pos,
                } => self.format_runtime_error(&message, start_pos, end_pos),
                Error::VmRuntimeError {
                    message,
                    instruction,
                } => format!("VM Runtime error at instruction {}: {}\n",instruction,  message),
                Error::ExternalError { message } => format!("External error: {}\n", message,),
            }),
        }
    }

    fn format_runtime_error(
        &self,
        message: &str,
        start_pos: &Position,
        end_pos: &Position,
    ) -> String {
        let excerpt_lines = self
            .script
            .lines()
            .skip(start_pos.line - 1)
            .take(end_pos.line - start_pos.line + 1)
            .collect::<Vec<_>>();

        let line_numbers = (start_pos.line..=end_pos.line)
            .map(|n| n.to_string())
            .collect::<Vec<_>>();

        let number_width = line_numbers.iter().max_by_key(|n| n.len()).unwrap().len();

        let padding = format!("{}", " ".repeat(number_width + 2));

        let excerpt = if excerpt_lines.len() == 1 {
            let mut excerpt = format!(
                " {:>width$} | {}\n",
                line_numbers.first().unwrap(),
                excerpt_lines.first().unwrap(),
                width = number_width
            );

            excerpt += &format!(
                "{}|{}",
                padding,
                format!(
                    "{}{}",
                    " ".repeat(start_pos.column),
                    "^".repeat(end_pos.column - start_pos.column)
                ),
            );

            excerpt
        } else {
            let mut excerpt = String::new();

            for (excerpt_line, line_number) in excerpt_lines.iter().zip(line_numbers.iter()) {
                excerpt += &format!(
                    " {:>width$} | {}\n",
                    line_number,
                    excerpt_line,
                    width = number_width
                );
            }

            excerpt
        };

        format!(
            "Runtime error: {message}\n --> {}:{}\n{padding}|\n{excerpt}",
            start_pos.line,
            start_pos.column,
            padding = padding,
            excerpt = excerpt,
            message = message
        )
    }
}
