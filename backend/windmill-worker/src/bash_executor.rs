use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
};

use anyhow::Result;
use async_recursion::async_recursion;
use regex::Regex;
use serde_json::{json, value::RawValue};
use sqlx::types::Json;
use tokio::process::Command;
use windmill_common::{error::Error, jobs::QueuedJob, worker::to_raw_value};
use windmill_queue::{append_logs, CanceledBy};

const BIN_BASH: &str = "/bin/bash";
const NSJAIL_CONFIG_RUN_BASH_CONTENT: &str = include_str!("../nsjail/run.bash.config.proto");
const NSJAIL_CONFIG_RUN_POWERSHELL_CONTENT: &str =
    include_str!("../nsjail/run.powershell.config.proto");

lazy_static::lazy_static! {
    static ref RE_POWERSHELL_IMPORTS: Regex = Regex::new(r#"^(?i)Import-Module(?-i)\s+(?:-Force\s+)?(?:-Name\s+)?(?:(?:"([^-\s"]+)")|(?:'([^-\s']+)')|([^-\s'"]+))"#).unwrap();
}

use crate::{
    common::{
        build_args_map, get_reserved_variables, handle_child, read_file, read_file_content,
        start_child_process, write_file,
    },
    AuthedClientBackgroundTask, DISABLE_NSJAIL, DISABLE_NUSER, HOME_ENV, NSJAIL_PATH, PATH_ENV,
    POWERSHELL_CACHE_DIR, POWERSHELL_PATH, TZ_ENV,
};

lazy_static::lazy_static! {

    pub static ref ANSI_ESCAPE_RE: Regex = Regex::new(r"\x1b\[[0-9;]*m").unwrap();
}

#[tracing::instrument(level = "trace", skip_all)]
pub async fn handle_bash_job(
    mem_peak: &mut i32,
    canceled_by: &mut Option<CanceledBy>,
    job: &QueuedJob,
    db: &sqlx::Pool<sqlx::Postgres>,
    client: &AuthedClientBackgroundTask,
    content: &str,
    job_dir: &str,
    shared_mount: &str,
    base_internal_url: &str,
    worker_name: &str,
    envs: HashMap<String, String>,
) -> Result<Box<RawValue>, Error> {
    let logs1 = "\n\n--- BASH CODE EXECUTION ---\n".to_string();
    append_logs(&job.id, &job.workspace_id, logs1, db).await;

    write_file(job_dir, "main.sh", &format!("set -e\n{content}")).await?;
    write_file(
        job_dir,
        "wrapper.sh",
        "set -o pipefail\nset -e\nmkfifo bp\ncat bp | tail -1 > ./result2.out &\n /bin/bash ./main.sh \"$@\" 2>&1 | tee bp\nwait $!",
    )
    .await?;

    let token = client.get_token().await;
    let mut reserved_variables = get_reserved_variables(job, &token, db).await?;
    reserved_variables.insert("RUST_LOG".to_string(), "info".to_string());

    let args = build_args_map(job, client, db).await?.map(Json);
    let job_args = if args.is_some() {
        args.as_ref()
    } else {
        job.args.as_ref()
    };

    let args_owned = windmill_parser_bash::parse_bash_sig(&content)?
        .args
        .iter()
        .map(|arg| {
            job_args
                .and_then(|x| x.get(&arg.name).map(|x| raw_to_string(x.get())))
                .unwrap_or_else(String::new)
        })
        .collect::<Vec<String>>();
    let args = args_owned.iter().map(|s| &s[..]).collect::<Vec<&str>>();
    let _ = write_file(job_dir, "result.json", "").await?;
    let _ = write_file(job_dir, "result.out", "").await?;
    let _ = write_file(job_dir, "result2.out", "").await?;

    let child = if !*DISABLE_NSJAIL {
        let _ = write_file(
            job_dir,
            "run.config.proto",
            &NSJAIL_CONFIG_RUN_BASH_CONTENT
                .replace("{JOB_DIR}", job_dir)
                .replace("{CLONE_NEWUSER}", &(!*DISABLE_NUSER).to_string())
                .replace("{SHARED_MOUNT}", shared_mount),
        )
        .await?;
        let mut cmd_args = vec![
            "--config",
            "run.config.proto",
            "--",
            "/bin/bash",
            "wrapper.sh",
        ];
        cmd_args.extend(args);
        let mut nsjail_cmd = Command::new(NSJAIL_PATH.as_str());
        nsjail_cmd
            .current_dir(job_dir)
            .env_clear()
            .envs(reserved_variables)
            .env("PATH", PATH_ENV.as_str())
            .env("BASE_INTERNAL_URL", base_internal_url)
            .args(cmd_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        start_child_process(nsjail_cmd, NSJAIL_PATH.as_str()).await?
    } else {
        let mut cmd_args = vec!["wrapper.sh"];
        cmd_args.extend(&args);
        let mut bash_cmd = Command::new(BIN_BASH);
        bash_cmd
            .current_dir(job_dir)
            .env_clear()
            .envs(envs)
            .envs(reserved_variables)
            .env("PATH", PATH_ENV.as_str())
            .env("BASE_INTERNAL_URL", base_internal_url)
            .env("HOME", HOME_ENV.as_str())
            .args(cmd_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        start_child_process(bash_cmd, BIN_BASH).await?
    };
    handle_child(
        &job.id,
        db,
        mem_peak,
        canceled_by,
        child,
        !*DISABLE_NSJAIL,
        worker_name,
        &job.workspace_id,
        "bash run",
        job.timeout,
        true,
    )
    .await?;

    let result_json_path = format!("{job_dir}/result.json");
    if let Ok(metadata) = tokio::fs::metadata(&result_json_path).await {
        if metadata.len() > 0 {
            return Ok(read_file(&result_json_path).await?);
        }
    }

    let result_out_path = format!("{job_dir}/result.out");
    if let Ok(metadata) = tokio::fs::metadata(&result_out_path).await {
        if metadata.len() > 0 {
            let result = read_file_content(&result_out_path).await?;
            return Ok(to_raw_value(&json!(result)));
        }
    }

    let result_out_path2 = format!("{job_dir}/result2.out");
    if tokio::fs::metadata(&result_out_path2).await.is_ok() {
        let result = read_file_content(&result_out_path2)
            .await?
            .trim()
            .to_string();
        return Ok(to_raw_value(&json!(result)));
    }

    Ok(to_raw_value(&json!(
        "No result.out, result2.out or result.json found"
    )))
}

fn raw_to_string(x: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(x) {
        Ok(serde_json::Value::String(x)) => x,
        Ok(x) => serde_json::to_string(&x).unwrap_or_else(|_| String::new()),
        _ => String::new(),
    }
}

fn parse_path(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    let mut result = PathBuf::new();

    if let Some(Component::RootDir) = components.peek() {
        result.push(components.next().unwrap());
    }

    for component in components {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            _ => result.push(component.as_os_str()),
        }
    }
    result
}

#[async_recursion]
pub async fn handle_powershell_deps(
    install_string: &mut String,
    installed_modules: &[String],
    visited_nodes: &mut HashSet<String>,
    source_file_name: &str,
    logs: &mut String,
    job_dir: &str,
    db: &sqlx::Pool<sqlx::Postgres>,
    content: &str,
) -> Result<String, Error> {
    let mut code_content = content.to_string();
    for line in content.lines() {
        for cap in RE_POWERSHELL_IMPORTS.captures_iter(line) {
            let raw_module = cap
                .get(1)
                .unwrap_or_else(|| cap.get(2).unwrap_or_else(|| cap.get(3).unwrap()))
                .as_str();
            let mut module = raw_module.to_string().replace(".ps1", "");
            if !installed_modules.contains(&module.to_lowercase()) {
                if module.starts_with("f/") || module.starts_with("u/") || module.starts_with('.') {
                    if !module.starts_with("f/")
                        && !module.starts_with("u/")
                        && module.starts_with('.')
                    {
                        let script_folder = Path::new(source_file_name)
                            .parent()
                            .unwrap()
                            .join(module.clone());
                        module = parse_path(&script_folder).to_str().unwrap().to_string();
                    }
                    if module == *source_file_name {
                        module = "main".to_string();
                    }
                    let file_name = format!("{}.ps1", &module.replace('/', "."));
                    let file_name_dot_reference = format!("./{}", file_name);
                    let whole_match = cap.get(0).unwrap().as_str();
                    let import_string = whole_match.replace(raw_module, &file_name_dot_reference);
                    code_content = code_content.replace(whole_match, &import_string);
                    if visited_nodes.contains(&module) {
                        continue;
                    }
                    visited_nodes.insert(module.clone());
                    let content = sqlx::query_scalar!(
                        "SELECT content FROM script where path = $1 ORDER BY created_at DESC",
                        &module
                    )
                    .fetch_optional(db)
                    .await?;
                    if let Some(content) = content {
                        if !Path::new(format!("{}/{}", job_dir, file_name).as_str()).exists() {
                            write_file(
                                job_dir,
                                &file_name,
                                &handle_powershell_deps(
                                    install_string,
                                    installed_modules,
                                    visited_nodes,
                                    source_file_name,
                                    logs,
                                    job_dir,
                                    db,
                                    &content,
                                )
                                .await?,
                            )
                            .await?;
                        }
                    }
                } else {
                    // instead of using Install-Module, we use Save-Module so that we can specify the installation path
                    logs.push_str(&format!("\n{} not found in cache", raw_module));
                    install_string.push_str(&format!(
                        "Save-Module -Path {} -Force {};",
                        POWERSHELL_CACHE_DIR, raw_module
                    ));
                }
            } else {
                logs.push_str(&format!("\n{} found in cache", raw_module));
            }
        }
    }
    return Ok(code_content);
}

#[tracing::instrument(level = "trace", skip_all)]
pub async fn handle_powershell_job(
    mem_peak: &mut i32,
    canceled_by: &mut Option<CanceledBy>,
    job: &QueuedJob,
    db: &sqlx::Pool<sqlx::Postgres>,
    client: &AuthedClientBackgroundTask,
    content: &str,
    job_dir: &str,
    shared_mount: &str,
    base_internal_url: &str,
    worker_name: &str,
    envs: HashMap<String, String>,
) -> Result<Box<RawValue>, Error> {
    let pwsh_args = {
        let args = build_args_map(job, client, db).await?.map(Json);
        let job_args = if args.is_some() {
            args.as_ref()
        } else {
            job.args.as_ref()
        };

        let args_owned = windmill_parser_bash::parse_powershell_sig(content)?
            .args
            .iter()
            .map(|arg| {
                (
                    arg.name.clone(),
                    job_args
                        .and_then(|x| x.get(&arg.name).map(|x| raw_to_string(x.get())))
                        .unwrap_or_else(String::new),
                )
            })
            .collect::<Vec<(String, String)>>();
        args_owned
            .iter()
            .flat_map(|(n, v)| vec![format!("--{n}"), format!("{v}")])
            .collect::<Vec<_>>()
    };

    let installed_modules = fs::read_dir(POWERSHELL_CACHE_DIR)?
        .filter_map(|x| {
            x.ok().map(|x| {
                x.path()
                    .display()
                    .to_string()
                    .split('/')
                    .last()
                    .unwrap_or_default()
                    .to_lowercase()
            })
        })
        .collect::<Vec<String>>();

    let mut install_string: String = String::new();
    let mut logs1 = String::new();
    let mut visited_nodes: HashSet<String> = HashSet::new();
    visited_nodes.insert("main".to_string());
    let mut code_content = handle_powershell_deps(
        &mut install_string,
        &installed_modules,
        &mut visited_nodes,
        job.script_path(),
        &mut logs1,
        job_dir,
        db,
        content,
    )
    .await?;

    if !install_string.is_empty() {
        logs1.push_str("\n\nInstalling modules...");
        append_logs(&job.id, &job.workspace_id, logs1, db).await;
        let child = Command::new("pwsh")
            .args(["-Command", &install_string])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        handle_child(
            &job.id,
            db,
            mem_peak,
            canceled_by,
            child,
            false,
            worker_name,
            &job.workspace_id,
            "powershell install",
            job.timeout,
            false,
        )
        .await?;
    }

    let mut logs2 = "".to_string();
    logs2.push_str("\n\n--- POWERSHELL CODE EXECUTION ---\n");
    append_logs(&job.id, &job.workspace_id, logs2, db).await;

    // make sure default (only allhostsallusers) modules are loaded, disable autoload (cache can be large to explore especially on cloud) and add /tmp/windmill/cache to PSModulePath
    let profile = format!(
        "$PSModuleAutoloadingPreference = 'None'
$PSModulePathBackup = $env:PSModulePath
$env:PSModulePath = ($Env:PSModulePath -split ':')[-1]
Get-Module -ListAvailable | Import-Module
$env:PSModulePath = \"{}:$PSModulePathBackup\"",
        POWERSHELL_CACHE_DIR
    );
    // make sure param() is first
    let param_match = windmill_parser_bash::RE_POWERSHELL_PARAM.find(&code_content);
    code_content = if let Some(param_match) = param_match {
        let param_match = param_match.as_str();
        format!(
            "{}\n{}\n{}",
            param_match,
            profile,
            code_content.replace(param_match, "")
        )
    } else {
        format!("{}\n{}", profile, code_content)
    };

    write_file(job_dir, "main.ps1", &code_content).await?;
    write_file(
        job_dir,
        "wrapper.sh",
        &format!("set -o pipefail\nset -e\nmkfifo bp\ncat bp | tail -1 > ./result2.out &\n{} -F ./main.ps1 \"$@\" 2>&1 | tee bp\nwait $!", POWERSHELL_PATH.as_str()),
    )
    .await?;
    let token = client.get_token().await;
    let mut reserved_variables = get_reserved_variables(job, &token, db).await?;
    reserved_variables.insert("RUST_LOG".to_string(), "info".to_string());

    let _ = write_file(job_dir, "result.json", "").await?;
    let _ = write_file(job_dir, "result.out", "").await?;
    let _ = write_file(job_dir, "result2.out", "").await?;

    let child = if !*DISABLE_NSJAIL {
        let _ = write_file(
            job_dir,
            "run.config.proto",
            &NSJAIL_CONFIG_RUN_POWERSHELL_CONTENT
                .replace("{JOB_DIR}", job_dir)
                .replace("{CLONE_NEWUSER}", &(!*DISABLE_NUSER).to_string())
                .replace("{SHARED_MOUNT}", shared_mount)
                .replace("{CACHE_DIR}", POWERSHELL_CACHE_DIR),
        )
        .await?;
        let mut cmd_args = vec![
            "--config",
            "run.config.proto",
            "--",
            "/bin/bash",
            "wrapper.sh",
        ];
        cmd_args.extend(pwsh_args.iter().map(|x| x.as_str()));
        Command::new(NSJAIL_PATH.as_str())
            .current_dir(job_dir)
            .env_clear()
            .envs(reserved_variables)
            .env("TZ", TZ_ENV.as_str())
            .env("PATH", PATH_ENV.as_str())
            .env("BASE_INTERNAL_URL", base_internal_url)
            .args(cmd_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?
    } else {
        let mut cmd_args = vec!["wrapper.sh"];
        cmd_args.extend(pwsh_args.iter().map(|x| x.as_str()));
        Command::new("/bin/bash")
            .current_dir(job_dir)
            .env_clear()
            .envs(envs)
            .envs(reserved_variables)
            .env("TZ", TZ_ENV.as_str())
            .env("PATH", PATH_ENV.as_str())
            .env("BASE_INTERNAL_URL", base_internal_url)
            .env("HOME", HOME_ENV.as_str())
            .args(cmd_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?
    };
    handle_child(
        &job.id,
        db,
        mem_peak,
        canceled_by,
        child,
        !*DISABLE_NSJAIL,
        worker_name,
        &job.workspace_id,
        "powershell run",
        job.timeout,
        false,
    )
    .await?;

    let result_out_path2 = format!("{job_dir}/result2.out");
    if tokio::fs::metadata(&result_out_path2).await.is_ok() {
        let result = read_file_content(&result_out_path2)
            .await?
            .trim()
            .to_string();
        return Ok(to_raw_value(&json!(result)));
    }

    Ok(to_raw_value(&json!(
        "No result.out, result2.out or result.json found"
    )))
}
