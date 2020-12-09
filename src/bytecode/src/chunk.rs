use {
    crate::InstructionReader,
    koto_parser::{ConstantPool, Span},
    std::{path::PathBuf, sync::Arc},
};

/// Debug information for a Koto program
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DebugInfo {
    source_map: Vec<(usize, Span)>,
    /// The source of the program that the debug info was derived from
    pub source: String,
}

impl DebugInfo {
    pub fn push(&mut self, ip: usize, span: Span) {
        if let Some(entry) = self.source_map.last() {
            if entry.1 == span {
                // Don't add entries with matching spans, a search is performed in
                // get_source_span which will find the correct span
                // for intermediate ips.
                return;
            }
        }
        self.source_map.push((ip, span));
    }

    /// Returns a source span for a given instruction pointer
    pub fn get_source_span(&self, ip: usize) -> Option<Span> {
        // Find the last entry with an ip less than or equal to the input
        // an upper_bound would nice here, but this isn't currently a performance sensitive function
        // so a scan through the entries will do.
        let mut result = None;
        for entry in self.source_map.iter() {
            if entry.0 <= ip {
                result = Some(entry.1);
            } else {
                break;
            }
        }
        result
    }
}

/// A compiled chunk of bytecode, along with its associated constants and metadata
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    /// The bytes representing the chunk's bytecode
    pub bytes: Vec<u8>,
    /// The constant data associated with the chunk's bytecode
    pub constants: ConstantPool,
    /// The constant string data associated with the chunk's bytecode
    pub string_constants_arc: Arc<str>,
    /// The path of the program's source file
    pub source_path: Option<PathBuf>,
    /// Debug information associated with the chunk's bytecode
    pub debug_info: DebugInfo,
}

impl Default for Chunk {
    fn default() -> Self {
        Self {
            bytes: vec![],
            constants: ConstantPool::default(),
            string_constants_arc: String::default().into(),
            source_path: None,
            debug_info: DebugInfo::default(),
        }
    }
}

impl Chunk {
    pub fn new(
        bytes: Vec<u8>,
        constants: ConstantPool,
        source_path: Option<PathBuf>,
        debug_info: DebugInfo,
    ) -> Self {
        Self {
            bytes,
            string_constants_arc: constants.string_data().into(),
            constants,
            source_path,
            debug_info,
        }
    }
}

/// Returns a [String] displaying the instructions contained in the compiled [Chunk]
pub fn chunk_to_string(chunk: Arc<Chunk>) -> String {
    let mut result = String::new();
    let mut reader = InstructionReader::new(chunk);
    let mut ip = reader.ip;

    while let Some(instruction) = reader.next() {
        result += &format!("{}\t{}\n", ip, &instruction.to_string());
        ip = reader.ip;
    }

    result
}

/// Returns a [String] displaying the annotated instructions contained in the compiled [Chunk]
pub fn chunk_to_string_annotated(chunk: Arc<Chunk>, source_lines: &[&str]) -> String {
    let mut result = String::new();
    let mut reader = InstructionReader::new(chunk);
    let mut ip = reader.ip;
    let mut span: Option<Span> = None;
    let mut first = true;

    while let Some(instruction) = reader.next() {
        let instruction_span = reader
            .chunk
            .debug_info
            .get_source_span(ip)
            .expect("Missing source span");

        let print_source_lines = if let Some(span) = span {
            instruction_span.start.line != span.start.line
        } else {
            true
        };

        if print_source_lines && !source_lines.is_empty() {
            if !first {
                result += "\n";
            }
            first = false;

            let line = instruction_span
                .start
                .line
                .min(source_lines.len() as u32)
                .max(1) as usize;
            result += &format!("|{}| {}\n", line.to_string(), source_lines[line - 1]);
            span = Some(instruction_span);
        }

        result += &format!("{}\t{:?}\n", ip, &instruction);
        ip = reader.ip;
    }

    result
}
