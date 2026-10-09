use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: Option<ModelConfig>,
    pub instructions: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<PathBuf>,
    pub timeout_seconds: u64,
    pub context_bytes: usize,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            model: String::new(),
            api_key_env: Some("OPENAI_API_KEY".into()),
            api_key_file: None,
            timeout_seconds: 120,
            context_bytes: 600_000,
        }
    }
}

impl ModelConfig {
    pub fn key(&self) -> Result<String> {
        let key = if let Some(path) = &self.api_key_file {
            fs::read_to_string(path).context("无法读取 API key 文件")?
        } else if let Some(name) = &self.api_key_env {
            std::env::var(name).with_context(|| format!("缺少环境变量 {name}"))?
        } else {
            bail!("需要 api_key_env 或 api_key_file");
        };
        if key.trim().is_empty() {
            bail!("API key 为空");
        }
        Ok(key.trim().to_owned())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UserConfig {
    pub shell: String,
    pub shell_args: Vec<String>,
    pub color: u8,
    pub abbreviation: String,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()),
            shell_args: vec!["-i".into()],
            color: 33,
            abbreviation: String::new(),
        }
    }
}

impl Config {
    pub fn load(project: &Path) -> Result<Self> {
        let file = project.join(".crew/config.toml");
        if !file.exists() {
            return Ok(Self::default());
        }
        toml::from_str(&fs::read_to_string(file)?).context("配置 TOML 无效")
    }
    pub fn load_user(project: &Path, user: &str) -> Result<UserConfig> {
        validate_name(user)?;
        let file = project.join(format!(".crew/users/{user}.toml"));
        let mut config: UserConfig = if file.exists() {
            toml::from_str(&fs::read_to_string(file)?).context("个人配置 TOML 无效")?
        } else {
            let color = 16 + user.bytes().fold(0u8, |a, b| a.wrapping_add(b)) % 200;
            UserConfig {
                color,
                ..UserConfig::default()
            }
        };
        if config.abbreviation.is_empty() {
            config.abbreviation = user.chars().take(2).collect();
        }
        Ok(config)
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 40
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || "_-".contains(c))
    {
        bail!("名称需要 1 至 40 个字母、数字、下划线或连字符");
    }
    Ok(())
}

pub fn socket_path(project: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let path = project.join(format!(".crew/{name}.sock"));
    if path.as_os_str().len() > 100 {
        bail!("项目路径过长，Unix socket 路径不能超过 100 字节");
    }
    Ok(path)
}

pub fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let next = path.with_extension("json.next");
    let file = fs::File::create(&next)?;
    serde_json::to_writer_pretty(&file, value)?;
    file.sync_all()?;
    fs::rename(next, path)?;
    fs::File::open(path.parent().expect("持久化文件必须有父目录"))?.sync_all()?;
    Ok(())
}
