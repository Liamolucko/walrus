//! Fuzz testing for `walrus`.

#![deny(missing_docs)]

use failure::ResultExt;
use rand::{Rng, SeedableRng};
use std::cmp;
use std::fmt;
use std::fs;
use std::marker::PhantomData;
use std::path::Path;
use std::time;
use walrus_tests_utils::{wasm_interp, wat2wasm};

/// `Ok(T)` or a `Err(failure::Error)`
pub type Result<T> = std::result::Result<T, failure::Error>;

#[derive(Copy, Clone, Debug)]
enum ValType {
    I32,
}

/// Anything that can generate WAT test cases for fuzzing.
pub trait TestCaseGenerator {
    /// The name of this test case generator.
    const NAME: &'static str;

    /// Whether we should interpret the generated test case before and after
    /// passing it through walrus in `wasm-interp` or not. This condition exists
    /// because `wasm-opt` can generate imports that `wasm-interp` does not know
    /// how to provide.
    const SHOULD_INTERPRET: bool;

    /// Generate a string of WAT deterministically using the given RNG seed and
    /// fuel.
    fn generate(seed: u64, fuel: usize) -> String;
}

/// Configuration for fuzzing.
pub struct Config<G: TestCaseGenerator> {
    _generator: PhantomData<G>,
    fuel: usize,
    timeout: u64,
    scratch: tempfile::NamedTempFile,
}

impl<G: TestCaseGenerator> Config<G> {
    /// The default fuel level.
    pub const DEFAULT_FUEL: usize = 64;

    /// The default timeout (in seconds).
    pub const DEFAULT_TIMEOUT_SECS: u64 = 5;

    /// Construct a new fuzzing configuration.
    pub fn new() -> Config<G> {
        let fuel = Self::DEFAULT_FUEL;
        let timeout = Self::DEFAULT_TIMEOUT_SECS;

        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("target")
            .join("walrus-fuzz");
        fs::create_dir_all(&dir).unwrap();
        let scratch = tempfile::NamedTempFile::new_in(dir).unwrap();

        Config {
            _generator: PhantomData,
            fuel,
            timeout,
            scratch,
        }
    }

    /// Set the fuel level.
    ///
    /// `fuel` must be greater than zero.
    pub fn set_fuel(mut self, fuel: usize) -> Config<G> {
        assert!(fuel > 0);
        self.fuel = fuel;
        self
    }

    fn gen_wat(&self, seed: u64) -> String {
        G::generate(seed, self.fuel)
    }

    fn wat2wasm(&self, wat: &str) -> Result<Vec<u8>> {
        fs::write(self.scratch.path(), wat).context("failed to write to scratch file")?;
        wat2wasm(self.scratch.path())
    }

    fn interp(&self, wasm: &[u8]) -> Result<String> {
        if G::SHOULD_INTERPRET {
            fs::write(self.scratch.path(), &wasm).context("failed to write to scratch file")?;
            wasm_interp(self.scratch.path())
        } else {
            Ok("".into())
        }
    }

    fn round_trip_through_walrus(&self, wasm: &[u8]) -> Result<Vec<u8>> {
        println!("parsing into walrus::Module");
        let module =
            walrus::Module::from_buffer(&wasm).context("walrus failed to parse the wasm buffer")?;
        println!("serializing walrus::Module back into wasm");
        let buf = module
            .emit_wasm()
            .context("walrus failed to serialize a module to wasm")?;
        Ok(buf)
    }

    fn run_one(&self, wat: &str) -> Result<()> {
        let wasm = self.wat2wasm(&wat)?;
        let expected = self.interp(&wasm)?;

        let walrus_wasm = self.round_trip_through_walrus(&wasm)?;
        let actual = self.interp(&walrus_wasm)?;

        if expected == actual {
            return Ok(());
        }

        Err(FailingTestCase {
            generator: G::NAME,
            wat: wat.to_string(),
            expected,
            actual,
        }
        .into())
    }

    /// Generate a wasm file and then compare its output in the reference
    /// interpreter before and after round tripping it through `walrus`.
    ///
    /// Returns the reduced failing test case, if any.
    pub fn run(&mut self) -> Result<()> {
        let start = time::Instant::now();
        let timeout = time::Duration::from_secs(self.timeout);
        let mut seed = rand::thread_rng().gen();
        let mut failing = Ok(());
        loop {
            println!("-----------------------------------------------------");

            let wat = self.gen_wat(seed);
            match self
                .run_one(&wat)
                .with_context(|_| format!("wat = {}", wat))
            {
                Ok(()) => {
                    // We reduced fuel as far as we could, so return the last
                    // failing test case.
                    if failing.is_err() {
                        return failing;
                    }

                    // Used all of our time, and didn't find any failing test cases.
                    if time::Instant::now().duration_since(start) > timeout {
                        assert!(failing.is_ok());
                        return Ok(());
                    }

                    // This RNG seed did not produce a failing test case, so choose
                    // a new one.
                    seed = rand::thread_rng().gen();
                    continue;
                }

                Err(e) => {
                    let e: failure::Error = e.into();
                    print_err(&e);
                    failing = Err(e);

                    // If we can try and reduce this test case with another
                    // iteration but with smaller fuel, do that. Otherwise
                    // return the failing test case.
                    if self.fuel > 1 {
                        self.fuel -= self.fuel / 10;
                    } else {
                        return failing;
                    }
                }
            }
        }
    }
}

/// A failing wasm test case where round tripping the wasm through walrus
/// produces an observably different execution in the reference interpreter.
#[derive(Clone, Debug)]
pub struct FailingTestCase {
    /// The WAT disassembly of the wasm test case.
    pub wat: String,

    /// The reference interpeter's output while interpreting the wasm *before* it
    /// has been round tripped through `walrus`.
    pub expected: String,

    /// The reference interpeter's output while interpreting the wasm *after* it
    /// has been round tripped through `walrus`.
    pub actual: String,

    /// The test case generator that created this failing test case.
    pub generator: &'static str,
}

impl fmt::Display for FailingTestCase {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(
            f,
            "\
Found a failing test case!

{wat}

BEFORE round tripping through walrus:

{before}

AFTER round tripping through walrus:

{after}

Here is a standalone test case:

----------------8<----------------8<----------------8<----------------
#[test]
fn test_name() {{
    walrus_fuzz::assert_round_trip_execution_is_same::<{generator}>(\"\\
{wat}\");
}}
----------------8<----------------8<----------------8<----------------
",
            wat = self.wat,
            before = self.expected,
            after = self.actual,
            generator = self.generator,
        )
    }
}

impl std::error::Error for FailingTestCase {}

/// Assert that the given WAT has the same execution trace before and after
/// round tripping it through walrus.
pub fn assert_round_trip_execution_is_same<G: TestCaseGenerator>(wat: &str) {
    let config = Config::<G>::new();
    let failing_test_case = config.run_one(wat);
    assert!(failing_test_case.is_ok());
}

/// A simple WAT generator.
pub struct WatGen {
    rng: rand::rngs::SmallRng,
    wat: String,
}

impl TestCaseGenerator for WatGen {
    const NAME: &'static str = "WatGen";

    const SHOULD_INTERPRET: bool = true;

    fn generate(seed: u64, fuel: usize) -> String {
        let rng = rand::rngs::SmallRng::seed_from_u64(seed);
        let wat = String::new();
        let mut g = WatGen { rng, wat };
        g.prefix();
        g.gen_instructions(fuel);
        g.suffix();
        g.wat
    }
}

impl WatGen {
    fn prefix(&mut self) {
        self.wat.push_str(
            "\
(module
  (import \"host\" \"print\" (func (param i32) (result i32)))
  (func (export \"$f\")
",
        );
    }

    fn suffix(&mut self) {
        self.wat.push_str("  ))");
    }

    fn gen_instructions(&mut self, fuel: usize) {
        assert!(fuel > 0);

        let mut stack = vec![];

        for _ in 0..fuel {
            self.op(&mut stack);
            if !stack.is_empty() {
                self.instr("call 0");
            }
        }

        for _ in stack {
            self.instr("call 0");
            self.instr("drop");
        }
    }

    fn instr_imm<S, I>(&mut self, operator: impl ToString, immediates: I)
    where
        S: AsRef<str>,
        I: IntoIterator<Item = S>,
    {
        self.wat.push_str("    ");
        self.wat.push_str(&operator.to_string());

        for op in immediates.into_iter() {
            self.wat.push_str(" ");
            self.wat.push_str(op.as_ref());
        }

        self.wat.push('\n');
    }

    fn instr(&mut self, operator: impl ToString) {
        self.instr_imm(operator, None::<String>);
    }

    fn op(&mut self, stack: &mut Vec<ValType>) {
        let arity = self.rng.gen_range(0, cmp::min(3, stack.len() + 1));
        match arity {
            0 => self.op_0(stack),
            1 => self.op_1(stack.pop().unwrap(), stack),
            2 => self.op_2(stack.pop().unwrap(), stack.pop().unwrap(), stack),
            _ => unreachable!(),
        }
    }

    fn op_0(&mut self, stack: &mut Vec<ValType>) {
        match self.rng.gen_range(0, 2) {
            0 => {
                let value = self.rng.gen::<i32>().to_string();
                self.instr_imm("i32.const", Some(value));
                stack.push(ValType::I32);
            }
            1 => {
                self.instr("nop");
            }
            _ => unreachable!(),
        }
    }

    fn op_1(&mut self, _operand: ValType, stack: &mut Vec<ValType>) {
        match self.rng.gen_range(0, 2) {
            0 => {
                self.instr("drop");
            }
            1 => {
                self.instr("i32.popcnt");
                stack.push(ValType::I32);
            }
            _ => unreachable!(),
        }
    }

    fn op_2(&mut self, _a: ValType, _b: ValType, stack: &mut Vec<ValType>) {
        match self.rng.gen_range(0, 2) {
            0 => {
                self.instr("i32.add");
                stack.push(ValType::I32);
            }
            1 => {
                self.instr("i32.mul");
                stack.push(ValType::I32);
            }
            _ => unreachable!(),
        }
    }
}

/// Use `wasm-opt -ttf` to generate fuzzing test cases.
pub struct WasmOptTtf;

impl TestCaseGenerator for WasmOptTtf {
    const NAME: &'static str = "WasmOptTtf";

    const SHOULD_INTERPRET: bool = false;

    fn generate(seed: u64, fuel: usize) -> String {
        let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);

        loop {
            let input: Vec<u8> = (0..fuel).map(|_| rng.gen()).collect();

            let input_tmp = tempfile::NamedTempFile::new().unwrap();
            fs::write(input_tmp.path(), input).unwrap();

            let wat = walrus_tests_utils::wasm_opt(
                input_tmp.path(),
                vec![
                    "-ttf",
                    "--emit-text",
                    // wasm-opt and wat2wasm seem to disagree on some of these.
                    "--disable-sign-ext",
                ],
            )
            .unwrap();
            if {
                // Only generate programs that wat2wasm can handle.
                let tmp = tempfile::NamedTempFile::new().unwrap();
                fs::write(tmp.path(), &wat).unwrap();
                wat2wasm(tmp.path()).is_ok()
            } {
                return String::from_utf8(wat).unwrap();
            }
        }
    }
}

fn print_err(e: &failure::Error) {
    eprintln!("Error:");
    for c in e.iter_chain() {
        eprintln!("  - {}", c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watgen_fuzz() {
        let mut config = Config::<WatGen>::new();
        if let Err(failing_test_case) = config.run() {
            print_err(&failing_test_case);
            panic!("Found a failing test case");
        }
    }

    #[test]
    fn wasm_opt_ttf_fuzz() {
        let mut config = Config::<WasmOptTtf>::new();
        config.timeout = 60 * 5;
        if let Err(failing_test_case) = config.run() {
            print_err(&failing_test_case);
            panic!("Found a failing test case");
        }
    }
}
