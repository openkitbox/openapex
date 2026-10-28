//! Env overrides of the LLM config keys. Own test binary (one process, one
//! test) because the process environment is global state.

use ox::config::Config;

#[test]
fn llm_env_overrides_win_over_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ox.toml");
    std::fs::write(
        &path,
        "cardinal_url = \"http://file.test\"\ncardinal_key = \"file-key\"\ndefault_model = \"cardinal/file\"\n",
    )
    .unwrap();

    // SAFETY: this binary holds a single test, so nothing else reads or
    // writes the environment concurrently.
    unsafe {
        std::env::set_var("OX_CARDINAL_URL", "http://env.test");
        std::env::set_var("OX_CARDINAL_KEY", "env-key");
        std::env::set_var("OX_DEFAULT_MODEL", "cardinal/env");
    }
    let config = Config::load_from(path).expect("config loads");
    unsafe {
        std::env::remove_var("OX_CARDINAL_URL");
        std::env::remove_var("OX_CARDINAL_KEY");
        std::env::remove_var("OX_DEFAULT_MODEL");
    }

    assert_eq!(config.llm.cardinal_url, "http://env.test");
    assert_eq!(config.llm.cardinal_key.as_deref(), Some("env-key"));
    assert_eq!(config.llm.default_model.as_deref(), Some("cardinal/env"));
}
