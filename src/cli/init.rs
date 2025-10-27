//! `init` — create the database and a commented config scaffold.

use std::path::Path;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::storage::{StoreHandle, schema};

/// Run init against the resolved config.
pub async fn run(cfg: &Config, force: bool) -> Result<()> {
    let config_path = Path::new("agos-memory.toml");
    if force || !config_path.exists() {
        if config_path.exists() && force {
            println!("overwriting {}", config_path.display());
        }
        std::fs::write(config_path, example_config(cfg))
            .map_err(|e| Error::Storage(format!("cannot write {}: {e}", config_path.display())))?;
        println!("wrote {}", config_path.display());
    } else {
        println!(
            "{} already exists (use --force to overwrite)",
            config_path.display()
        );
    }

    let store = StoreHandle::open(cfg, crate::defaults::READ_POOL_SIZE).await?;

    // Warm the schema and confirm the vector extension end to end.
    let v = store.vec_version().await?;
    let integrity = store.integrity().await?;
    let sv = store.schema_version().await?;

    println!("db:            {}", cfg.db_path.display());
    println!(
        "schema:        v{sv} (supported v{})",
        schema::SCHEMA_VERSION
    );
    println!("sqlite-vec:    {v}");
    println!("integrity:     {integrity}");
    println!("init complete");
    Ok(())
}

fn example_config(cfg: &Config) -> String {
    format!(
        r#"# agos-memory configuration
# Precedence: defaults < this file < AGOS_MEMORY_* env < CLI flags.

db_path = "{}"
agent_id = "{}"

# Embeddings via an OpenAI-compatible endpoint. In AGOS, point this at
# agos-proxy to inherit masking, prompt caching, and cost accounting.
[embed]
provider = "openai_compat"     # openai_compat | none (keyword-only degraded mode)
base_url = "{}"
model = "{}"
# api_key = "sk-..."           # or AGOS_MEMORY_EMBED_API_KEY; never commit

# Extraction LLM (used from v0.2.0).
[llm]
base_url = "{}"
model = "{}"
# api_key = "sk-..."           # or AGOS_MEMORY_LLM_API_KEY

[server]
# Streamable-HTTP MCP + JSON API bind address (v0.5.0).
# Non-loopback binds REQUIRE a token and refuse to start without one.
bind = "{}"
# token = "change-me-to-a-long-random-value"
"#,
        cfg.db_path.display(),
        cfg.agent_id,
        cfg.embed.base_url,
        cfg.embed.model,
        cfg.llm.base_url,
        cfg.llm.model,
        cfg.server.bind,
    )
}
