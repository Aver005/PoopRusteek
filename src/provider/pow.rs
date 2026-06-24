use base64::Engine as _;
use crate::debug_log;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use wasmtime::{Engine, Linker, Memory, Module, Store, TypedFunc};

const WASM_ASSET_NAME: &str = "sha3_wasm_bg.7b9ca65ddd.wasm";
static WASM_RUNTIME: OnceLock<Result<WasmPowRuntime, String>> = OnceLock::new();

struct WasmPowRuntime {
    engine: Engine,
    module: Module,
    path: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PowChallengeResponse {
    pub data: PowResponseData,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PowResponseData {
    pub biz_data: PowBizData,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PowBizData {
    pub challenge: PowChallengeData,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PowChallengeData {
    pub algorithm: String,
    pub challenge: String,
    #[serde(deserialize_with = "deserialize_string_or_number")]
    pub difficulty: String,
    pub salt: String,
    pub signature: String,
    pub target_path: String,
    pub expire_at: u64,
}

fn deserialize_string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(text) => Ok(text),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected string or number, got {other}"
        ))),
    }
}

#[derive(Debug, Serialize)]
pub struct PowSolution {
    pub algorithm: String,
    pub answer: u64,
    pub challenge: String,
    pub difficulty: String,
    pub expire_at: u64,
    pub salt: String,
    pub signature: String,
    pub target_path: String,
}

pub fn solve_pow(challenge: &PowChallengeData) -> Option<PowSolution> {
    if challenge.algorithm != "DeepSeekHashV1" {
        tracing::warn!("Unknown PoW algorithm: {}", challenge.algorithm);
        return None;
    }

    let difficulty: f64 = challenge.difficulty.parse().ok()?;
    let prefix = format!("{}_{}_", challenge.salt, challenge.expire_at);

    let runtime = get_wasm_runtime().ok()?;
    debug_log::log(
        "pow.solver.runtime",
        format!("using wasm asset {}", runtime.path.display()),
    );

    let answer = runtime
        .solve(&challenge.challenge, &prefix, difficulty)
        .map_err(|error| {
            tracing::warn!("PoW wasm solve failed: {error}");
            debug_log::log("pow.solver.solve", format!("error={error}"));
        })
        .ok()??;
    debug_log::log(
        "pow.solver.solve",
        format!(
            "challenge={} difficulty={} answer={answer}",
            challenge.challenge, challenge.difficulty
        ),
    );

    Some(PowSolution {
        algorithm: challenge.algorithm.clone(),
        answer,
        challenge: challenge.challenge.clone(),
        difficulty: challenge.difficulty.clone(),
        expire_at: challenge.expire_at,
        salt: challenge.salt.clone(),
        signature: challenge.signature.clone(),
        target_path: challenge.target_path.clone(),
    })
}

fn get_wasm_runtime() -> Result<&'static WasmPowRuntime, String> {
    match WASM_RUNTIME.get_or_init(WasmPowRuntime::load) {
        Ok(runtime) => Ok(runtime),
        Err(error) => {
            debug_log::log("pow.solver.runtime", format!("init_error={error}"));
            Err(error.clone())
        }
    }
}

impl WasmPowRuntime {
    fn load() -> Result<Self, String> {
        let path = resolve_wasm_path()
            .ok_or_else(|| "WASM file not found in any candidate location".to_string())?;
        let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
        let engine = Engine::default();
        let module = Module::from_binary(&engine, &bytes).map_err(|error| error.to_string())?;

        Ok(Self { engine, module, path })
    }

    fn solve(&self, challenge: &str, prefix: &str, difficulty: f64) -> Result<Option<u64>, String> {
        let mut store = Store::new(&self.engine, ());
        let mut linker = Linker::new(&self.engine);
        linker
            .func_wrap("env", "abort", || {})
            .map_err(|error| error.to_string())?;

        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|error| error.to_string())?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| "Missing WASM export: memory".to_string())?;
        let add_to_stack = instance
            .get_typed_func::<i32, i32>(&mut store, "__wbindgen_add_to_stack_pointer")
            .map_err(|error| error.to_string())?;
        let alloc = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "__wbindgen_export_0")
            .map_err(|error| error.to_string())?;
        let wasm_solve = instance
            .get_typed_func::<(i32, i32, i32, i32, i32, f64), ()>(&mut store, "wasm_solve")
            .map_err(|error| error.to_string())?;

        let retptr = add_to_stack
            .call(&mut store, -16)
            .map_err(|error| error.to_string())?;

        let result = (|| {
            let (ptr_challenge, len_challenge) =
                encode_string(&mut store, &memory, &alloc, challenge)?;
            let (ptr_prefix, len_prefix) = encode_string(&mut store, &memory, &alloc, prefix)?;

            wasm_solve
                .call(
                    &mut store,
                    (
                        retptr,
                        ptr_challenge,
                        len_challenge,
                        ptr_prefix,
                        len_prefix,
                        difficulty,
                    ),
                )
                .map_err(|error| error.to_string())?;

            let status = read_i32(&store, &memory, retptr as usize)?;
            let value = read_f64(&store, &memory, (retptr + 8) as usize)?;
            if status == 0 {
                return Ok(None);
            }

            Ok(Some(value.floor() as u64))
        })();

        let _ = add_to_stack.call(&mut store, 16);
        result
    }
}

pub fn encode_solution(solution: &PowSolution) -> String {
    let json = serde_json::to_string(solution).unwrap();
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

fn encode_string(
    store: &mut Store<()>,
    memory: &Memory,
    alloc: &TypedFunc<(i32, i32), i32>,
    text: &str,
) -> Result<(i32, i32), String> {
    let bytes = text.as_bytes();
    let length = i32::try_from(bytes.len()).map_err(|error| error.to_string())?;
    let ptr = alloc
        .call(&mut *store, (length, 1))
        .map_err(|error| error.to_string())?;
    memory
        .write(&mut *store, ptr as usize, bytes)
        .map_err(|error| error.to_string())?;
    Ok((ptr, length))
}

fn read_i32(store: &Store<()>, memory: &Memory, offset: usize) -> Result<i32, String> {
    let mut bytes = [0u8; 4];
    memory
        .read(store, offset, &mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(i32::from_le_bytes(bytes))
}

fn read_f64(store: &Store<()>, memory: &Memory, offset: usize) -> Result<f64, String> {
    let mut bytes = [0u8; 8];
    memory
        .read(store, offset, &mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(f64::from_le_bytes(bytes))
}

fn resolve_wasm_path() -> Option<PathBuf> {
    let asset_rel = Path::new("assets").join(WASM_ASSET_NAME);
    let manifest_candidate = Path::new(env!("CARGO_MANIFEST_DIR")).join(&asset_rel);
    let cwd_candidate = std::env::current_dir().ok().map(|dir| dir.join(&asset_rel));
    let exe_candidate = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join(&asset_rel)));

    [Some(manifest_candidate), cwd_candidate, exe_candidate]
        .into_iter()
        .flatten()
        .find(|path| path.exists())
}
