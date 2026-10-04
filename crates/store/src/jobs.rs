//! Local metadata for remote background jobs. The job itself (and its log)
//! lives on the remote host, so it survives daemon restarts.

use serde::{Deserialize, Serialize};
use xssh_core::error::{Error, Result};
use xssh_core::paths::Paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: String,
    pub host: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub started_at: String,
    /// Remote directory holding `<id>.log`, `<id>.exit`, `<id>.pid`.
    pub remote_dir: String,
    #[serde(default)]
    pub pid: Option<u32>,
    /// The process start time as the host reports it (`/proc/PID/stat` field 22, or
    /// `ps -o lstart`): a pid with another start time was reused by an unrelated process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start: Option<String>,
    #[serde(default)]
    pub sudo: bool,
    /// Last state seen (`running`, `exited`, `killed`, `lost`): finished jobs need no query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
}

impl JobRecord {
    pub fn log_path(&self) -> String {
        format!("{}/{}.log", self.remote_dir, self.id)
    }
    pub fn exit_path(&self) -> String {
        format!("{}/{}.exit", self.remote_dir, self.id)
    }
    /// A final state was recorded (the job cannot run again).
    pub fn finished(&self) -> bool {
        matches!(self.last_state.as_deref(), Some("exited" | "killed" | "lost"))
    }
    /// Seconds since the job started (0 when the timestamp cannot be parsed).
    pub fn age_secs(&self) -> u64 {
        chrono::DateTime::parse_from_rfc3339(&self.started_at)
            .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds().max(0) as u64)
            .unwrap_or(0)
    }
}

pub fn save(paths: &Paths, rec: &JobRecord) -> Result<()> {
    let f = paths.jobs_dir().join(format!("{}.json", rec.id));
    std::fs::write(f, serde_json::to_vec_pretty(rec)?)?;
    Ok(())
}

pub fn load(paths: &Paths, id: &str) -> Result<JobRecord> {
    let f = paths.jobs_dir().join(format!("{id}.json"));
    let s = std::fs::read_to_string(&f).map_err(|_| Error::not_found(format!("unknown job '{id}'")).hint("run `xssh job list --all`"))?;
    Ok(serde_json::from_str(&s)?)
}

pub fn remove(paths: &Paths, id: &str) -> Result<()> {
    let _ = std::fs::remove_file(paths.jobs_dir().join(format!("{id}.json")));
    Ok(())
}

pub fn list(paths: &Paths) -> Result<Vec<JobRecord>> {
    let mut out = vec![];
    for e in std::fs::read_dir(paths.jobs_dir())? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "json")
            && let Ok(s) = std::fs::read_to_string(&p)
            && let Ok(r) = serde_json::from_str::<JobRecord>(&s)
        {
            out.push(r);
        }
    }
    out.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    Ok(out)
}

/// Delete records of finished jobs older than `days` (0 keeps everything). Returns how many.
pub fn gc(paths: &Paths, days: u64) -> usize {
    if days == 0 {
        return 0;
    }
    let Ok(all) = list(paths) else { return 0 };
    let mut n = 0;
    for r in all.iter().filter(|r| r.finished() && r.age_secs() > days * 86400) {
        if remove(paths, &r.id).is_ok() {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, started: &str, state: Option<&str>) -> JobRecord {
        JobRecord {
            id: id.into(),
            host: "h".into(),
            command: "true".into(),
            name: None,
            cwd: None,
            started_at: started.into(),
            remote_dir: "/home/u/.xssh/jobs".into(),
            pid: Some(1),
            pid_start: None,
            sudo: false,
            last_state: state.map(String::from),
            exit_code: None,
        }
    }

    #[test]
    fn gc_removes_only_old_finished_records() {
        let home = std::env::temp_dir().join(format!("xssh-jobs-gc-{}", std::process::id()));
        let paths = Paths::resolve(Some(&home)).unwrap();
        let old = "2000-01-01T00:00:00Z";
        let now = chrono::Utc::now().to_rfc3339();
        save(&paths, &rec("jold", old, Some("exited"))).unwrap();
        save(&paths, &rec("jrun", old, Some("running"))).unwrap();
        save(&paths, &rec("jnew", &now, Some("exited"))).unwrap();
        assert_eq!(gc(&paths, 0), 0);
        assert_eq!(gc(&paths, 30), 1);
        let ids: Vec<String> = list(&paths).unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["jrun", "jnew"]);
        let _ = std::fs::remove_dir_all(&home);
    }
}
