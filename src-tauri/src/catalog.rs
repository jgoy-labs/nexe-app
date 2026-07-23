//! Model catalog Tauri command for the onboarding wizard.
//!
//! Fetches the model catalog from the remote manifest URL (5 s timeout)
//! and falls back to the embedded JSON when the network is unavailable.
//! All parsing happens in Rust — the HTML frontend never sees raw JSON from
//! an untrusted source.

use serde::{Deserialize, Serialize};

/// A single model entry returned to the onboarding frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub name: String,
    pub params: String,
    /// RAM requirement in GB (f64 to match JSON export from Python catalog).
    pub ram_gb: f64,
    pub disk_gb: f64,
    /// Available backends, e.g. ["MLX", "Ollama", "llama.cpp"].
    pub backends: Vec<String>,
    /// Feature flags, e.g. ["vision", "thinking", "catalan"].
    pub flags: Vec<String>,
    /// Origin string, e.g. "Google DeepMind".
    pub origin: String,
    pub ollama: Option<String>,
    pub mlx: Option<String>,
    pub gguf: Option<String>,
    /// Gated model indicator from HuggingFace ("manual", "auto", or None).
    /// Exposed so the frontend can show a 🔒 badge in Step 2.
    #[serde(default)]
    pub gated: Option<String>,
    /// Optional license URL for gated models.
    #[serde(default)]
    pub license_url: Option<String>,
}

/// Embedded fallback catalog (built into the binary at compile time).
const FALLBACK_CATALOG: &str = include_str!("../resources/catalog_fallback.json");

/// Remote manifest URL. Fetched with a 5 s timeout; falls back to embedded on any error.
///
/// Points to the `catalog-bootstrap` branch of the public `server-nexe` repo,
/// which hosts the latest model catalog. On any fetch error the embedded
/// catalog is used as a fallback.
const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/jgoy-labs/server-nexe/catalog-bootstrap/docs/catalog.json";

/// Cap màxim del cos del manifest remot abans de parsejar-lo (mirall
/// d'`auth::fetch_from_sidecar`). El catàleg real fa desenes de KB; 4 MiB
/// és marge de sobres. Sense cap, `resp.json()` bufferitza el cos sencer,
/// de manera que un endpoint compromès o un MITM podria retornar molts GB
/// i exhaurir la memòria del procés abans que serde ni arrenqui.
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;

/// Resultat d'avaluar el cos del manifest remot després d'aplicar el cap.
/// Qualsevol variant diferent de `Ok` provoca fallback a l'embedat.
enum ManifestOutcome {
    /// Manifest vàlid amb almenys un model.
    Ok(Vec<CatalogModel>),
    /// Cos vàlid però sense cap model → fallback.
    Empty,
    /// Cos que supera el cap de mida (mida real ja llegida) → fallback.
    TooLarge,
    /// Error de deserialització → fallback.
    ParseError,
}

/// Cert si el `Content-Length` declarat supera el cap: permet descartar
/// el cos abans de llegir-lo. Si el servidor no declara mida, retorna
/// `false` i la mida real es comprova després a `evaluate_manifest_body`.
fn declared_too_large(content_length: Option<u64>) -> bool {
    content_length.is_some_and(|declared| declared > MAX_CATALOG_BYTES)
}

/// Avalua el cos ja llegit: aplica el cap sobre la mida REAL (defensa
/// contra respostes sense `Content-Length` o que menteixin) i, si passa,
/// el parseja amb `serde_json::from_slice`.
fn evaluate_manifest_body(bytes: &[u8]) -> ManifestOutcome {
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return ManifestOutcome::TooLarge;
    }
    match serde_json::from_slice::<Vec<CatalogModel>>(bytes) {
        Ok(models) if !models.is_empty() => ManifestOutcome::Ok(models),
        Ok(_) => ManifestOutcome::Empty,
        Err(_) => ManifestOutcome::ParseError,
    }
}

/// Return the model catalog for the onboarding wizard.
///
/// Called via `invoke("fetch_catalog")` from the frontend (Step 2).
/// Uses the remote manifest when available; falls back to the embedded JSON silently.
/// Does not require the sidecar.
#[tauri::command]
pub async fn fetch_catalog() -> Vec<CatalogModel> {
    // Attempt remote fetch with timeout.
    if let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        match client.get(MANIFEST_URL).send().await {
            Ok(resp) => {
                let status = resp.status();
                // OOM guard: descarta pel Content-Length declarat abans de
                // drenar el cos a memòria.
                if declared_too_large(resp.content_length()) {
                    tracing::warn!(
                        url = %MANIFEST_URL,
                        status = %status,
                        content_length = ?resp.content_length(),
                        cap = MAX_CATALOG_BYTES,
                        "catalog: remote manifest Content-Length over cap — falling back to embedded",
                    );
                } else {
                    match resp.bytes().await {
                        Ok(bytes) => match evaluate_manifest_body(&bytes) {
                            ManifestOutcome::Ok(models) => {
                                tracing::info!(
                                    url = %MANIFEST_URL,
                                    count = models.len(),
                                    "catalog: remote manifest consumed",
                                );
                                return models;
                            }
                            ManifestOutcome::TooLarge => {
                                tracing::warn!(
                                    url = %MANIFEST_URL,
                                    status = %status,
                                    read = bytes.len(),
                                    cap = MAX_CATALOG_BYTES,
                                    "catalog: remote manifest body over cap — falling back to embedded",
                                );
                            }
                            ManifestOutcome::Empty => {
                                tracing::warn!(
                                    url = %MANIFEST_URL,
                                    status = %status,
                                    "catalog: remote manifest empty — falling back to embedded",
                                );
                            }
                            ManifestOutcome::ParseError => {
                                tracing::warn!(
                                    url = %MANIFEST_URL,
                                    status = %status,
                                    "catalog: remote manifest deserialise failed — falling back to embedded",
                                );
                            }
                        },
                        Err(e) => {
                            tracing::warn!(
                                url = %MANIFEST_URL,
                                status = %status,
                                error = %e,
                                "catalog: remote manifest body read failed — falling back to embedded",
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    url = %MANIFEST_URL,
                    error = %e,
                    "catalog: remote manifest fetch failed — falling back to embedded",
                );
            }
        }
    }
    // Silent fallback to embedded catalog.
    tracing::info!("catalog: using embedded fallback");
    serde_json::from_str(FALLBACK_CATALOG).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_catalog_is_valid_json() {
        let models: Vec<CatalogModel> =
            serde_json::from_str(FALLBACK_CATALOG).expect("embedded catalog must be valid JSON");
        assert!(
            !models.is_empty(),
            "embedded catalog must have at least one model"
        );
    }

    #[test]
    fn fallback_catalog_models_have_required_fields() {
        let models: Vec<CatalogModel> = serde_json::from_str(FALLBACK_CATALOG).unwrap();
        for m in &models {
            assert!(!m.name.is_empty(), "model name must not be empty");
            assert!(m.ram_gb > 0.0, "model ram_gb must be > 0 for {}", m.name);
            assert!(m.disk_gb > 0.0, "model disk_gb must be > 0 for {}", m.name);
            assert!(
                !m.backends.is_empty(),
                "model {} must have at least one backend",
                m.name
            );
        }
    }

    #[test]
    fn fallback_catalog_has_minimum_models() {
        let models: Vec<CatalogModel> = serde_json::from_str(FALLBACK_CATALOG).unwrap();
        assert!(
            models.len() >= 3,
            "fallback catalog must have >= 3 models, got {}",
            models.len()
        );
    }

    #[test]
    fn gated_field_preserved_through_deserialization() {
        // Verify gated field survives Rust deserialization (Serde
        // used to silently drop it because CatalogModel lacked the field).
        // Uses a synthetic JSON literal so the test does not depend on any
        // specific entry of the fallback catalog (which may shrink/grow).
        let json = r#"[{
            "name": "Test Model",
            "params": "4B",
            "ram_gb": 4.0,
            "disk_gb": 3.3,
            "backends": ["MLX", "Ollama"],
            "flags": [],
            "origin": "Test",
            "ollama": "test:4b",
            "mlx": null,
            "gguf": null,
            "gated": "manual",
            "license_url": "https://example.test/license"
        }]"#;
        let models: Vec<CatalogModel> = serde_json::from_str(json).unwrap();
        assert_eq!(models[0].gated.as_deref(), Some("manual"));
        assert_eq!(
            models[0].license_url.as_deref(),
            Some("https://example.test/license"),
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // Cap de mida del manifest remot (NEXE-APP-WSG-002 / NEXE-APP-WSE-001).
    // Sense cap, `resp.json()` bufferitzava el cos sencer: un endpoint
    // compromès/MITM podia retornar molts GB i exhaurir la memòria. El fix
    // reflecteix `auth::fetch_from_sidecar`: cap per Content-Length + mida
    // real, i fallback a l'embedat si es passa.
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn declared_too_large_rejects_over_cap_content_length() {
        assert!(
            declared_too_large(Some(MAX_CATALOG_BYTES + 1)),
            "un Content-Length per sobre del cap s'ha de rebutjar",
        );
    }

    #[test]
    fn declared_too_large_accepts_at_or_below_cap_and_missing() {
        assert!(!declared_too_large(Some(MAX_CATALOG_BYTES)));
        assert!(!declared_too_large(Some(1024)));
        // Sense Content-Length no es pot decidir aquí; la mida real ho tanca.
        assert!(!declared_too_large(None));
    }

    #[test]
    fn evaluate_manifest_body_rejects_oversize_real_body() {
        // Cos que menteix o no declara mida però supera el cap: la mida real
        // el frena abans de serde.
        let oversize = vec![b' '; (MAX_CATALOG_BYTES + 1) as usize];
        assert!(
            matches!(evaluate_manifest_body(&oversize), ManifestOutcome::TooLarge),
            "un cos real per sobre del cap s'ha de rebutjar sense parsejar",
        );
    }

    #[test]
    fn evaluate_manifest_body_parses_valid_catalog() {
        let json = br#"[{
            "name": "Test Model",
            "params": "4B",
            "ram_gb": 4.0,
            "disk_gb": 3.3,
            "backends": ["MLX"],
            "flags": [],
            "origin": "Test",
            "ollama": "test:4b",
            "mlx": null,
            "gguf": null
        }]"#;
        match evaluate_manifest_body(json) {
            ManifestOutcome::Ok(models) => {
                assert_eq!(models.len(), 1);
                assert_eq!(models[0].name, "Test Model");
            }
            _ => panic!("un manifest vàlid i no buit ha de retornar Ok"),
        }
    }

    #[test]
    fn evaluate_manifest_body_flags_empty_and_parse_error() {
        assert!(matches!(
            evaluate_manifest_body(b"[]"),
            ManifestOutcome::Empty
        ));
        assert!(matches!(
            evaluate_manifest_body(b"{ not json"),
            ManifestOutcome::ParseError
        ));
    }

    #[test]
    fn fallback_catalog_non_gated_models_have_none() {
        let models: Vec<CatalogModel> = serde_json::from_str(FALLBACK_CATALOG).unwrap();
        // Non-gated models (e.g. Qwen3.5 4B) must deserialize with gated=None
        let non_gated = models.iter().find(|m| m.name == "Qwen3.5 4B");
        if let Some(m) = non_gated {
            assert!(
                m.gated.is_none(),
                "Qwen3.5 4B must not be gated, got {:?}",
                m.gated
            );
        }
    }
}
