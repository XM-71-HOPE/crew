use crate::session::{Edit, Id};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub struct Files {
    reads: BTreeMap<(Id, PathBuf), Option<String>>,
}

impl Files {
    pub fn resolve(cwd: &Path, path: &str) -> Result<PathBuf> {
        let path = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            cwd.join(path)
        };
        if path.exists() {
            return Ok(path.canonicalize()?);
        }
        let parent = path.parent().context("文件缺少父目录")?.canonicalize()?;
        Ok(parent.join(path.file_name().context("文件缺少名称")?))
    }
    pub fn authorized(path: &Path, allowed: &[PathBuf]) -> bool {
        allowed.iter().any(|p| path.starts_with(p))
    }
    pub fn read(&mut self, agent: Id, path: &Path, start: usize, end: usize) -> Result<String> {
        if !path.exists() {
            self.reads.insert((agent, path.into()), None);
            return Ok("文件不存在；已记录创建前的上下文".into());
        }
        if fs::metadata(path)?.len() > 2_000_000 {
            bail!("文件超过 2 MB，请使用外部工具检查");
        }
        let content = fs::read_to_string(path).context("文件不是有效 UTF-8 文本")?;
        self.reads
            .insert((agent, path.into()), Some(hash(content.as_bytes())));
        Ok(content
            .lines()
            .enumerate()
            .filter(|(i, _)| i + 1 >= start.max(1) && *i < end)
            .map(|(i, line)| format!("{}: {line}", i + 1))
            .collect::<Vec<_>>()
            .join("\n"))
    }
    pub fn edit(
        &mut self,
        agent: Id,
        id: Id,
        path: &Path,
        operation: Operation,
    ) -> Result<(Edit, String, String, String)> {
        let before = if path.exists() {
            Some(fs::read_to_string(path)?)
        } else {
            None
        };
        let current = before.as_ref().map(|s| hash(s.as_bytes()));
        let read = self
            .reads
            .get(&(agent, path.into()))
            .context("必须先读取文件上下文")?;
        if *read != current {
            bail!("文件在读取后发生修改，请重新读取；本次没有写入");
        }
        let after = match operation {
            Operation::Replace { old, new } => {
                let text = before.as_ref().context("文件不存在")?;
                if old.is_empty() || text.matches(&old).count() != 1 {
                    bail!("old 需要恰好匹配一次；本次没有写入");
                }
                text.replacen(&old, &new, 1)
            }
            Operation::Patch(patch) => diffy::apply(
                before.as_deref().context("文件不存在")?,
                &diffy::Patch::from_str(&patch).context("unified diff 无效")?,
            )
            .context("补丁上下文不匹配")?,
            Operation::Create(content) => {
                if before.is_some() {
                    bail!("文件已经存在；本次没有写入");
                }
                content
            }
        };
        if after.len() > 2_000_000 {
            bail!("编辑结果超过 2 MB");
        }
        // 再次检查外部并发修改；工作目录按设计不加文件锁。
        let latest = if path.exists() {
            Some(hash(&fs::read(path)?))
        } else {
            None
        };
        if latest != current {
            bail!("文件在编辑前发生修改；本次没有写入");
        }
        match &before {
            Some(_) => {
                fs::write(path, &after)?;
            }
            None => {
                use std::io::Write;
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?
                    .write_all(after.as_bytes())?;
            }
        }
        let after_hash = hash(after.as_bytes());
        self.reads
            .insert((agent, path.into()), Some(after_hash.clone()));
        let diff = diffy::create_patch(before.as_deref().unwrap_or(""), &after).to_string();
        Ok((
            Edit {
                id,
                path: path.into(),
                before: before.clone(),
                after_hash,
                undone: false,
            },
            before.unwrap_or_default(),
            after,
            diff,
        ))
    }
    pub fn undo(edit: &mut Edit) -> Result<()> {
        if edit.undone {
            bail!("修改已经撤销");
        }
        if !edit.path.exists() || hash(&fs::read(&edit.path)?) != edit.after_hash {
            bail!("文件有后续修改，不能撤销；请使用 Git 或人工处理");
        }
        if let Some(before) = &edit.before {
            fs::write(&edit.path, before)?;
        } else {
            fs::remove_file(&edit.path)?;
        }
        edit.undone = true;
        Ok(())
    }
    pub fn list(path: &Path) -> Result<String> {
        let mut names = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let suffix = if entry.file_type()?.is_dir() { "/" } else { "" };
            names.push(format!("{name}{suffix}"));
        }
        names.sort();
        Ok(names.join("\n"))
    }
    pub fn search(path: &Path, pattern: &str) -> Result<String> {
        let re = regex::Regex::new(pattern)?;
        let mut paths = Vec::new();
        walk(path, &mut paths)?;
        let mut hits = Vec::new();
        for path in paths {
            if fs::metadata(&path)?.len() > 2_000_000 {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
                Err(e) => return Err(e.into()),
            };
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    hits.push(format!("{}:{}:{line}", path.display(), i + 1));
                    if hits.len() >= 500 {
                        return Ok(hits.join("\n") + "\n结果达到 500 条限制");
                    }
                }
            }
        }
        Ok(hits.join("\n"))
    }
    pub fn hashes(path: &Path) -> Result<BTreeMap<PathBuf, String>> {
        let mut paths = Vec::new();
        walk(path, &mut paths)?;
        let mut hashes = BTreeMap::new();
        for path in paths {
            let mut file = fs::File::open(&path)?;
            let mut digest = Sha256::new();
            let mut buffer = [0; 65536];
            loop {
                let length = file.read(&mut buffer)?;
                if length == 0 {
                    break;
                }
                digest.update(&buffer[..length]);
            }
            hashes.insert(path, format!("{:x}", digest.finalize()));
        }
        Ok(hashes)
    }
}

pub enum Operation {
    Replace { old: String, new: String },
    Patch(String),
    Create(String),
}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn walk(path: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        paths.push(path.into());
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if [".crew", ".git", "target"].iter().any(|skip| name == *skip) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            walk(&entry.path(), paths)?;
        } else if kind.is_file() {
            paths.push(entry.path());
        }
    }
    Ok(())
}
