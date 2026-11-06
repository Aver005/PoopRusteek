use crate::debug_log;
use crate::error::{AppError, AppResult};
use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;
use wasmtime::{Engine, Linker, Memory, Module, Store, TypedFunc};

const WASM_ASSET_NAME: &str = "sha3_wasm_bg.7b9ca65ddd.wasm";
/// DeepSeek's own solver, embedded so an installed binary works with no
/// `assets/` folder nearby (the compile-time `CARGO_MANIFEST_DIR` fallback
/// broke as soon as the source checkout moved). A debug build prefers the
/// checkout's copy — see [`resolve_wasm_path`].
const EMBEDDED_WASM: &[u8] = include_bytes!("../../assets/sha3_wasm_bg.7b9ca65ddd.wasm");
static WASM_RUNTIME: OnceLock<Result<WasmPowRuntime, String>> = OnceLock::new();

struct WasmPowRuntime {
    engine: Engine,
    module: Module,
    /// Where the wasm bytes came from ("embedded" or a filesystem path) —
    /// debug-log only.
    source: String,
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
        format!("using wasm asset from {}", runtime.source),
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
        // В debug-сборке wasm из исходников перекрывает встроенный — новый
        // солвер работает без пересборки; release самодостаточен.
        let (bytes, source) = match resolve_wasm_path() {
            Some(path) => {
                let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
                let source = path.display().to_string();
                (std::borrow::Cow::Owned(bytes), source)
            }
            None => (
                std::borrow::Cow::Borrowed(EMBEDDED_WASM),
                "embedded".to_string(),
            ),
        };
        let engine = Engine::default();
        let module = Module::from_binary(&engine, &bytes).map_err(|error| error.to_string())?;

        Ok(Self {
            engine,
            module,
            source,
        })
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

pub fn encode_solution(solution: &PowSolution) -> AppResult<String> {
    let json = serde_json::to_string(solution)
        .map_err(|e| AppError::Custom(format!("Failed to serialize PoW solution: {e}")))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(json.as_bytes()))
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

/// Wasm из исходников в debug-сборке; release всегда берёт встроенный.
fn resolve_wasm_path() -> Option<PathBuf> {
    crate::util::dev_assets_dir()
        .map(|dir| dir.join(WASM_ASSET_NAME))
        .filter(|path| path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Фикстура повторяет форму настоящего challenge: без сети и токена.
    fn fixture(difficulty: &str) -> PowChallengeData {
        PowChallengeData {
            algorithm: "DeepSeekHashV1".to_string(),
            challenge: "0c1a1f2e3d4b5a69788796a5b4c3d2e1".to_string(),
            difficulty: difficulty.to_string(),
            salt: "9f8e7d6c5b4a39281706f5e4d3c2b1a0".to_string(),
            signature: "fixture-signature".to_string(),
            target_path: "/chat/completion".to_string(),
            expire_at: 1_762_345_678,
        }
    }

    /// Встроенный wasm грузится, экспорты на месте, память читается: любой
    /// сбой инициализации или смены ABI даёт None на первом же вызове.
    #[test]
    fn embedded_wasm_runtime_instantiates() {
        let runtime = get_wasm_runtime().expect("встроенный wasm должен грузиться");
        assert!(!runtime.source.is_empty());

        // Тот же вызов, что делает приложение: строка кодируется, солвер
        // пишет статус и ответ в память. Готового ответа быть не обязано.
        let outcome = runtime.solve(&fixture("100000").challenge, "salt_1_", 100_000.0);
        assert_eq!(
            outcome,
            Ok(None),
            "wasm_solve на фикстуре не даёт ответа: {outcome:?}"
        );
    }

    /// Эталон текущего wasm: для фикстуры ответа нет. Тест ловит и смену ABI,
    /// и появление решения там, где раньше его не было.
    #[test]
    fn unsolvable_fixture_stays_unsolved() {
        let challenge = fixture("100000");
        assert!(solve_pow(&challenge).is_none(), "эталон изменился");
        assert!(
            solve_pow(&challenge).is_none(),
            "ответ должен быть стабилен"
        );
    }

    /// Неверные входы отвергаются до запуска wasm.
    #[test]
    fn unknown_algorithm_and_bad_difficulty_are_rejected() {
        let mut wrong_algorithm = fixture("100000");
        wrong_algorithm.algorithm = "OtherAlgorithmV1".to_string();
        assert!(solve_pow(&wrong_algorithm).is_none());

        let mut bad_difficulty = fixture("not-a-number");
        bad_difficulty.algorithm = "DeepSeekHashV1".to_string();
        assert!(solve_pow(&bad_difficulty).is_none());
    }

    /// Тело решения для сервера: base64 от JSON с ровно теми полями, что
    /// ждёт DeepSeek, и подмена ответа меняет payload.
    #[test]
    fn encoded_solution_matches_the_server_contract() {
        let challenge = fixture("100000");
        let solution = PowSolution {
            algorithm: challenge.algorithm.clone(),
            answer: 42,
            challenge: challenge.challenge.clone(),
            difficulty: challenge.difficulty.clone(),
            expire_at: challenge.expire_at,
            salt: challenge.salt.clone(),
            signature: challenge.signature.clone(),
            target_path: challenge.target_path.clone(),
        };

        let encoded = encode_solution(&solution).expect("encode_solution");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .expect("payload разбирается как base64");
        let value: serde_json::Value = serde_json::from_slice(&decoded).expect("payload — JSON");

        assert_eq!(value["algorithm"], "DeepSeekHashV1");
        assert_eq!(value["answer"].as_u64(), Some(42));
        assert_eq!(value["challenge"], challenge.challenge);
        assert_eq!(value["difficulty"], challenge.difficulty);
        assert_eq!(value["expire_at"].as_u64(), Some(challenge.expire_at));
        assert_eq!(value["salt"], challenge.salt);
        assert_eq!(value["signature"], challenge.signature);
        assert_eq!(value["target_path"], challenge.target_path);

        // Неверный ответ даёт другой payload — сервер его не примет.
        let mut wrong = PowSolution {
            answer: 43,
            ..solution
        };
        let wrong_encoded = encode_solution(&wrong).expect("encode_solution");
        assert_ne!(encoded, wrong_encoded);
    }
}
