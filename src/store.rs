use crate::{
    media::{default_paper_sizes, PaperSize},
    printing::{self, PrintOptions, PrintOutcome},
};
use fs2::FileExt;
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

pub(crate) const MAX_PDF: usize = 100 * 1024 * 1024;
pub(crate) const ANONYMOUS_DEVICE: &str = "anonymous";
const IPP_DOCUMENT_TIMEOUT_SECS: i64 = 120;
const QUOTA: u64 = 2 * 1024 * 1024 * 1024;
type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn random_hex() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedPrinter {
    pub id: String,
    pub name: String,
    pub queue: String,
    pub enabled: bool,
    pub duplex: bool,
    pub max_copies: u32,
    #[serde(default = "default_paper_sizes")]
    pub paper_sizes: Vec<PaperSize>,
    #[serde(default)]
    pub default_paper: PaperSize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub revoked: bool,
}
#[derive(Serialize)]
pub struct CreatedDevice {
    pub device: Device,
    pub token: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub id: i64,
    pub device_id: String,
    pub printer_id: String,
    pub queue: String,
    pub copies: u32,
    pub sides: String,
    pub paper: PaperSize,
    pub color_mode: String,
    pub state: String,
    pub detail: String,
    pub created_at: i64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ConnectionEvent {
    pub timestamp: i64,
    pub client_ip: Option<String>,
    pub method: String,
    pub path: String,
    pub operation: Option<u16>,
    pub http_status: u16,
    pub ipp_status: Option<u16>,
    pub detail: String,
}
#[derive(Serialize)]
pub struct ServiceStatus {
    pub running: bool,
    pub simulation: bool,
    pub address: Option<String>,
    pub discovery_active: bool,
    pub discovery_error: Option<String>,
    pub authentication_required: bool,
    pub active_authentication_required: bool,
    pub authentication_restart_required: bool,
}
pub struct Bridge {
    pub(crate) db: Mutex<Connection>,
    root: PathBuf,
    simulation: bool,
    sumatra: PathBuf,
    pub(crate) running: AtomicBool,
    authentication_required: AtomicBool,
    configured_authentication_required: AtomicBool,
    connection_events: Mutex<VecDeque<ConnectionEvent>>,
    service: Mutex<Option<(String, tokio::sync::oneshot::Sender<()>)>>,
    discovery: Mutex<Option<crate::discovery::Discovery>>,
    discovery_start_error: Mutex<Option<String>>,
    worker_active: AtomicBool,
    server_active: AtomicBool,
    _lock: File,
}
impl Bridge {
    pub fn open(path: impl AsRef<Path>, simulation: bool, sumatra: PathBuf) -> Result<Arc<Self>> {
        let root = path.as_ref().join(if simulation {
            "simulation"
        } else {
            "production"
        });
        fs::create_dir_all(root.join("documents")).map_err(err)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("instance.lock"))
            .map_err(err)?;
        lock.try_lock_exclusive()
            .map_err(|_| "此数据目录已由另一个实例使用".to_string())?;
        let db = Connection::open(root.join("bridge.sqlite3")).map_err(err)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS printers(id TEXT PRIMARY KEY, config TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS devices(id TEXT PRIMARY KEY,name TEXT NOT NULL,hash TEXT NOT NULL,revoked INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS jobs(id INTEGER PRIMARY KEY AUTOINCREMENT,device_id TEXT NOT NULL,printer_id TEXT NOT NULL,queue TEXT NOT NULL,copies INTEGER NOT NULL,sides TEXT NOT NULL,state TEXT NOT NULL,detail TEXT NOT NULL DEFAULT '',created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,file TEXT NOT NULL,digest TEXT NOT NULL,color_mode TEXT NOT NULL DEFAULT 'auto');
            CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state,id);").map_err(err)?;
        let columns = db
            .prepare("PRAGMA table_info(jobs)")
            .map_err(err)?
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(err)?;
        if !columns.iter().any(|column| column == "paper") {
            db.execute(
                "ALTER TABLE jobs ADD COLUMN paper TEXT NOT NULL DEFAULT 'A4'",
                [],
            )
            .map_err(err)?;
        }
        if !columns.iter().any(|column| column == "color_mode") {
            db.execute(
                "ALTER TABLE jobs ADD COLUMN color_mode TEXT NOT NULL DEFAULT 'auto'",
                [],
            )
            .map_err(err)?;
        }
        db.execute("UPDATE jobs SET state='uncertain',detail='应用退出前正在调用打印程序，请检查 Windows 队列及出纸情况',updated_at=? WHERE state='processing'", [now()]).map_err(err)?;
        db.execute("DELETE FROM devices WHERE revoked=1", [])
            .map_err(err)?;
        let authentication_required = db
            .query_row(
                "SELECT value FROM settings WHERE key='authentication_required'",
                [],
                |row| row.get::<_, bool>(0),
            )
            .optional()
            .map_err(err)?
            .unwrap_or(true);
        let bridge = Arc::new(Self {
            db: Mutex::new(db),
            root,
            simulation,
            sumatra,
            running: AtomicBool::new(false),
            authentication_required: AtomicBool::new(authentication_required),
            configured_authentication_required: AtomicBool::new(authentication_required),
            connection_events: Mutex::new(VecDeque::new()),
            service: Mutex::new(None),
            discovery: Mutex::new(None),
            discovery_start_error: Mutex::new(None),
            worker_active: AtomicBool::new(false),
            server_active: AtomicBool::new(false),
            _lock: lock,
        });
        bridge.recover_files()?;
        Ok(bridge)
    }
    fn recover_files(&self) -> Result<()> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT id,file,state FROM jobs").map_err(err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(err)?;
        let mut known = std::collections::HashSet::new();
        for row in rows {
            let (id, file, state) = row.map_err(err)?;
            known.insert(file.clone());
            if matches!(state.as_str(), "pending" | "pending-final-document")
                && !self.root.join("documents").join(&file).is_file()
            {
                db.execute("UPDATE jobs SET state='failed',detail='恢复时文档缺失',updated_at=? WHERE id=?",params![now(),id]).map_err(err)?;
            }
        }
        for entry in fs::read_dir(self.root.join("documents")).map_err(err)? {
            let entry = entry.map_err(err)?;
            if entry.file_type().map_err(err)?.is_file()
                && !known.contains(&entry.file_name().to_string_lossy().to_string())
            {
                fs::remove_file(entry.path()).map_err(err)?;
            }
        }
        Ok(())
    }
    pub fn list_printers(&self) -> Result<Vec<SharedPrinter>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT config FROM printers ORDER BY id")
            .map_err(err)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(err)?;
        rows.map(|row| serde_json::from_str(&row.map_err(err)?).map_err(err))
            .collect()
    }
    pub(crate) fn printer(&self, id: &str) -> Result<SharedPrinter> {
        self.list_printers()?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| "打印机不存在".into())
    }
    pub fn save_printer(&self, p: SharedPrinter) -> Result<()> {
        if p.id.is_empty()
            || p.id.len() > 64
            || !p
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || p.name.trim().is_empty()
            || p.name.len() > 255
            || p.queue.trim().is_empty()
            || p.queue.contains('\0')
            || p.paper_sizes.is_empty()
            || !p.paper_sizes.contains(&p.default_paper)
            || p.paper_sizes
                .iter()
                .enumerate()
                .any(|(index, size)| p.paper_sizes[..index].contains(size))
            || !(1..=99).contains(&p.max_copies)
        {
            return Err("打印机配置无效：ID 仅允许字母、数字、横线和下划线；份数为 1–99；纸张须非空、无重复且包含默认纸张".into());
        }
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT config FROM printers").map_err(err)?;
        for row in stmt.query_map([], |r| r.get::<_, String>(0)).map_err(err)? {
            let old: SharedPrinter = serde_json::from_str(&row.map_err(err)?).map_err(err)?;
            if old.id != p.id && old.queue.to_lowercase() == p.queue.to_lowercase() {
                return Err("此 Windows 队列已有共享绑定".into());
            }
            if old.id == p.id && old.queue != p.queue {
                let n:i64=db.query_row("SELECT COUNT(*) FROM jobs WHERE printer_id=? AND state IN ('pending','pending-document','pending-final-document','processing','uncertain')",[&p.id],|r|r.get(0)).map_err(err)?;
                if n > 0 {
                    return Err("请先处理原绑定的未完成任务".into());
                }
            }
        }
        db.execute("INSERT INTO printers(id,config) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET config=excluded.config",params![p.id,serde_json::to_string(&p).map_err(err)?]).map_err(err)?;
        drop(stmt);
        drop(db);
        self.refresh_discovery();
        Ok(())
    }
    fn refresh_discovery(&self) {
        let service = self.service.lock().unwrap();
        let Some(address) = service
            .as_ref()
            .and_then(|(address, _)| address.parse::<std::net::SocketAddr>().ok())
        else {
            return;
        };
        if !self.running.load(Ordering::SeqCst) {
            return;
        }
        let auth = self.authentication_required();
        let old = self.discovery.lock().unwrap().take();
        drop(old);
        let result = self
            .list_printers()
            .and_then(|printers| crate::discovery::Discovery::start(&printers, address, auth));
        match result {
            Ok(discovery) => {
                *self.discovery.lock().unwrap() = Some(discovery);
                *self.discovery_start_error.lock().unwrap() = None;
            }
            Err(error) => {
                *self.discovery.lock().unwrap() = None;
                *self.discovery_start_error.lock().unwrap() = Some(error);
            }
        }
    }
    pub fn create_device(&self, name: String) -> Result<CreatedDevice> {
        if name.trim().is_empty() || name.len() > 100 {
            return Err("设备名称长度须为 1–100 字节".into());
        }
        let device = Device {
            id: random_hex()[..16].to_string(),
            name,
            revoked: false,
        };
        let token = random_hex();
        self.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO devices(id,name,hash) VALUES(?,?,?)",
                params![device.id, device.name, hash(token.as_bytes())],
            )
            .map_err(err)?;
        Ok(CreatedDevice { device, token })
    }
    pub fn list_devices(&self) -> Result<Vec<Device>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT id,name,revoked FROM devices WHERE revoked=0 ORDER BY name")
            .map_err(err)?;
        let result = stmt
            .query_map([], |r| {
                Ok(Device {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    revoked: r.get(2)?,
                })
            })
            .map_err(err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(err);
        result
    }
    pub fn revoke_device(&self, id: &str) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM devices WHERE id=?", [id])
            .map_err(err)?;
        Ok(())
    }
    pub(crate) fn authenticate(&self, id: &str, token: &str) -> bool {
        let db = self.db.lock().unwrap();
        let expected: Option<String> = db
            .query_row(
                "SELECT hash FROM devices WHERE id=? AND revoked=0",
                [id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        expected
            .map(|h| bool::from(h.as_bytes().ct_eq(hash(token.as_bytes()).as_bytes())))
            .unwrap_or(false)
    }
    pub fn authentication_required(&self) -> bool {
        self.authentication_required.load(Ordering::SeqCst)
    }
    pub fn set_authentication_required(&self, required: bool) -> Result<()> {
        let _service = self.service.lock().unwrap();
        self.db.lock().unwrap().execute("INSERT INTO settings(key,value) VALUES('authentication_required',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [required]).map_err(err)?;
        self.configured_authentication_required
            .store(required, Ordering::SeqCst);
        if !self.running.load(Ordering::SeqCst) {
            self.authentication_required
                .store(required, Ordering::SeqCst);
        }
        Ok(())
    }
    pub fn list_jobs(&self) -> Result<Vec<Job>> {
        let db = self.db.lock().unwrap();
        let mut stmt=db.prepare("SELECT id,device_id,printer_id,queue,copies,sides,state,detail,created_at,paper,color_mode FROM jobs ORDER BY id DESC LIMIT 1000").map_err(err)?;
        let result = stmt
            .query_map([], job_row)
            .map_err(err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(err);
        result
    }
    pub fn clear_job_history(&self) -> Result<usize> {
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT file FROM jobs WHERE file<>'' AND state IN ('submitted','simulated','failed','canceled','completed')")
            .map_err(err)?;
        for row in stmt.query_map([], |r| r.get::<_, String>(0)).map_err(err)? {
            let path = self.root.join("documents").join(row.map_err(err)?);
            match fs::remove_file(path) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(err(error)),
            }
        }
        db.execute(
            "DELETE FROM jobs WHERE state IN ('submitted','simulated','failed','canceled','completed')",
            [],
        )
        .map_err(err)
    }
    pub(crate) fn job(&self, id: i64) -> Result<Job> {
        let db = self.db.lock().unwrap();
        db.query_row("SELECT id,device_id,printer_id,queue,copies,sides,state,detail,created_at,paper,color_mode FROM jobs WHERE id=?",[id],job_row).map_err(|_|"任务不存在".into())
    }
    pub(crate) fn device_jobs(
        &self,
        device: &str,
        printer: &str,
        which: &str,
        limit: i32,
    ) -> Result<Vec<Job>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT id,device_id,printer_id,queue,copies,sides,state,detail,created_at,paper,color_mode FROM jobs
            WHERE device_id=? AND printer_id=? AND (?='all' OR
                CASE WHEN state IN ('submitted','simulated','failed','canceled','completed') THEN 'completed' ELSE 'not-completed' END=?)
            ORDER BY id DESC LIMIT ?").map_err(err)?;
        let result = stmt
            .query_map(
                params![device, printer, which, which, limit.clamp(1, 20)],
                job_row,
            )
            .map_err(err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(err);
        result
    }
    pub fn has_active_print(&self) -> Result<bool> {
        self.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE state='processing')",
                [],
                |r| r.get(0),
            )
            .map_err(err)
    }
    pub fn cancel_job(&self, id: i64) -> Result<()> {
        let n=self.db.lock().unwrap().execute("UPDATE jobs SET state='canceled',detail='执行前取消',updated_at=? WHERE id=? AND state IN ('pending','pending-document','pending-final-document')",params![now(),id]).map_err(err)?;
        if n == 1 {
            Ok(())
        } else {
            Err("仅可取消等待中的任务".into())
        }
    }
    pub(crate) fn validate_options(
        p: &SharedPrinter,
        copies: u32,
        sides: &str,
        paper: PaperSize,
        color_mode: &str,
    ) -> Result<()> {
        if !p.enabled {
            return Err("打印机已禁用".into());
        }
        if copies == 0 || copies > p.max_copies {
            return Err("不支持的份数".into());
        }
        if !p.paper_sizes.contains(&paper) {
            return Err("不支持的纸张尺寸".into());
        }
        if sides != "one-sided"
            && (!p.duplex || !matches!(sides, "two-sided-long-edge" | "two-sided-short-edge"))
        {
            return Err("不支持的单双面选项".into());
        }
        if !matches!(color_mode, "auto" | "color" | "monochrome") {
            return Err("不支持的颜色模式".into());
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn accept(
        &self,
        device: &str,
        printer: &str,
        copies: u32,
        sides: &str,
        paper: PaperSize,
        document: &[u8],
    ) -> Result<Job> {
        self.accept_format(
            device,
            printer,
            copies,
            sides,
            paper,
            "auto",
            "application/pdf",
            document,
        )
    }
    pub(crate) fn accept_format(
        &self,
        device: &str,
        printer: &str,
        copies: u32,
        sides: &str,
        paper: PaperSize,
        color_mode: &str,
        mime: &str,
        document: &[u8],
    ) -> Result<Job> {
        let pdf = crate::document::normalize(document, mime)
            .map_err(|error| format!("文档格式错误：{error}"))?;
        self.accept_pdf(device, printer, copies, sides, paper, color_mode, &pdf)
    }
    fn accept_pdf(
        &self,
        device: &str,
        printer: &str,
        copies: u32,
        sides: &str,
        paper: PaperSize,
        color_mode: &str,
        pdf: &[u8],
    ) -> Result<Job> {
        validate_pdf(pdf).map_err(|error| format!("文档格式错误：{error}"))?;
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction().map_err(err)?;
        if !self.running.load(Ordering::SeqCst) {
            return Err("服务未启动".into());
        }
        let active: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM devices WHERE id=? AND revoked=0)",
                [device],
                |r| r.get(0),
            )
            .map_err(err)?;
        if !active && !(device == ANONYMOUS_DEVICE && !self.authentication_required()) {
            return Err("设备令牌已撤销".into());
        }
        let config: String = tx
            .query_row("SELECT config FROM printers WHERE id=?", [printer], |r| {
                r.get(0)
            })
            .map_err(err)?;
        let p: SharedPrinter = serde_json::from_str(&config).map_err(err)?;
        Self::validate_options(&p, copies, sides, paper, color_mode)?;
        let count:i64=tx.query_row("SELECT COUNT(*) FROM jobs WHERE state IN ('pending','pending-document','pending-final-document','processing','uncertain') AND device_id=?",[device],|r|r.get(0)).map_err(err)?;
        let queue_count:i64=tx.query_row("SELECT COUNT(*) FROM jobs WHERE state IN ('pending','pending-document','pending-final-document','processing','uncertain') AND printer_id=?",[printer],|r|r.get(0)).map_err(err)?;
        if count >= 20 || queue_count >= 100 {
            return Err("等待任务达到容量限制".into());
        }
        let dir = self.root.join("documents");
        let used = fs::read_dir(&dir).map_err(err)?.try_fold(0u64, |sum, e| {
            Ok::<_, String>(sum + e.map_err(err)?.metadata().map_err(err)?.len())
        })?;
        if used + pdf.len() as u64 > QUOTA
            || fs2::available_space(&dir).map_err(err)? < pdf.len() as u64 + 1024 * 1024 * 1024
        {
            return Err("磁盘容量不足".into());
        }
        let name = format!("{}.pdf", random_hex());
        let path = dir.join(&name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(err)?;
        if let Err(e) = file.write_all(pdf).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&path);
            return Err(err(e));
        }
        let inserted=tx.execute("INSERT INTO jobs(device_id,printer_id,queue,copies,sides,paper,color_mode,state,created_at,updated_at,file,digest) VALUES(?,?,?,?,?,?,?,'pending',?,?,?,?)",params![device,printer,p.queue,copies,sides,paper.name(),color_mode,now(),now(),name,hash(pdf)]);
        if let Err(e) = inserted {
            let _ = fs::remove_file(&path);
            return Err(err(e));
        }
        let id = tx.last_insert_rowid();
        tx.commit().map_err(err)?;
        drop(db);
        self.job(id)
    }
    pub(crate) fn create_ipp_job(
        &self,
        device: &str,
        printer: &str,
        copies: u32,
        sides: &str,
        paper: PaperSize,
        color_mode: &str,
    ) -> Result<Job> {
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction().map_err(err)?;
        if !self.running.load(Ordering::SeqCst) {
            return Err("服务未启动".into());
        }
        let active: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM devices WHERE id=? AND revoked=0)",
                [device],
                |r| r.get(0),
            )
            .map_err(err)?;
        if !active && !(device == ANONYMOUS_DEVICE && !self.authentication_required()) {
            return Err("设备令牌已撤销".into());
        }
        let config: String = tx
            .query_row("SELECT config FROM printers WHERE id=?", [printer], |r| {
                r.get(0)
            })
            .map_err(err)?;
        let p: SharedPrinter = serde_json::from_str(&config).map_err(err)?;
        Self::validate_options(&p, copies, sides, paper, color_mode)?;
        let count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state IN ('pending','pending-document','pending-final-document','processing','uncertain') AND device_id=?",
                [device],
                |r| r.get(0),
            )
            .map_err(err)?;
        let queue_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state IN ('pending','pending-document','pending-final-document','processing','uncertain') AND printer_id=?",
                [printer],
                |r| r.get(0),
            )
            .map_err(err)?;
        if count >= 20 || queue_count >= 100 {
            return Err("等待任务达到容量限制".into());
        }
        let timestamp = now();
        tx.execute(
            "INSERT INTO jobs(device_id,printer_id,queue,copies,sides,paper,color_mode,state,detail,created_at,updated_at,file,digest) VALUES(?,?,?,?,?,?,?,'pending-document','等待文档',?,?,'','')",
            params![device, printer, p.queue, copies, sides, paper.name(), color_mode, timestamp, timestamp],
        )
        .map_err(err)?;
        let id = tx.last_insert_rowid();
        tx.commit().map_err(err)?;
        drop(db);
        self.job(id)
    }
    pub(crate) fn send_ipp_document(
        &self,
        device: &str,
        id: i64,
        document: &[u8],
        mime: &str,
        last_document: bool,
    ) -> Result<Job> {
        if !last_document && document.is_empty() {
            return Err("last-document=false 必须附带文档".into());
        }
        let pdf = if document.is_empty() {
            None
        } else {
            let pdf = crate::document::normalize(document, mime)
                .map_err(|error| format!("文档格式错误：{error}"))?;
            validate_pdf(&pdf).map_err(|error| format!("文档格式错误：{error}"))?;
            Some(pdf)
        };
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction().map_err(err)?;
        let job = tx
            .query_row(
                "SELECT id,device_id,printer_id,queue,copies,sides,state,detail,created_at,paper,color_mode FROM jobs WHERE id=?",
                [id],
                job_row,
            )
            .optional()
            .map_err(err)?
            .ok_or_else(|| "任务不存在".to_string())?;
        if job.device_id != device {
            return Err("任务不存在".into());
        }
        self.validate_ipp_target(&tx, device, &job)?;
        let updated_at: i64 = tx
            .query_row("SELECT updated_at FROM jobs WHERE id=?", [id], |r| r.get(0))
            .map_err(err)?;
        if matches!(
            job.state.as_str(),
            "pending-document" | "pending-final-document"
        ) && now() - updated_at > IPP_DOCUMENT_TIMEOUT_SECS
        {
            tx.execute(
                "UPDATE jobs SET state='failed',detail='等待文档超时',updated_at=? WHERE id=?",
                params![now(), id],
            )
            .map_err(err)?;
            tx.commit().map_err(err)?;
            return Err("等待文档超时".into());
        }
        let result = match (job.state.as_str(), pdf.as_deref(), last_document) {
            ("pending-document", None, true) => {
                tx.execute(
                    "UPDATE jobs SET state='completed',detail='未提交文档',updated_at=? WHERE id=? AND state='pending-document'",
                    params![now(), id],
                )
                .map_err(err)?;
                None
            }
            ("pending-final-document", None, true) => {
                tx.execute(
                    "UPDATE jobs SET state='pending',detail='',updated_at=? WHERE id=? AND state='pending-final-document'",
                    params![now(), id],
                )
                .map_err(err)?;
                None
            }
            ("pending-document", Some(pdf), last_document) => {
                let dir = self.root.join("documents");
                let used = fs::read_dir(&dir)
                    .map_err(err)?
                    .try_fold(0u64, |sum, entry| {
                        Ok::<_, String>(sum + entry.map_err(err)?.metadata().map_err(err)?.len())
                    })?;
                if used + pdf.len() as u64 > QUOTA
                    || fs2::available_space(&dir).map_err(err)?
                        < pdf.len() as u64 + 1024 * 1024 * 1024
                {
                    return Err("磁盘容量不足".into());
                }
                let file_name = format!("{}.pdf", random_hex());
                let path = dir.join(&file_name);
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(err)?;
                if let Err(error) = file.write_all(pdf).and_then(|_| file.sync_all()) {
                    let _ = fs::remove_file(&path);
                    return Err(err(error));
                }
                let next_state = if last_document {
                    "pending"
                } else {
                    "pending-final-document"
                };
                let changed = tx.execute(
                    "UPDATE jobs SET state=?,detail=?,updated_at=?,file=?,digest=? WHERE id=? AND state='pending-document'",
                    params![next_state, if last_document { "" } else { "等待最后文档标记" }, now(), file_name, hash(pdf), id],
                );
                match changed {
                    Ok(1) => Some(path),
                    Ok(_) => {
                        let _ = fs::remove_file(path);
                        return Err("任务状态已变化".into());
                    }
                    Err(error) => {
                        let _ = fs::remove_file(path);
                        return Err(err(error));
                    }
                }
            }
            ("pending-final-document", Some(_), _) => {
                return Err("不支持一个任务包含多个文档".into());
            }
            _ => return Err("Only a job awaiting a document can receive one".into()),
        };
        if let Err(error) = tx.commit() {
            if let Some(path) = result {
                let _ = fs::remove_file(path);
            }
            return Err(err(error));
        }
        drop(db);
        self.job(id)
    }
    fn validate_ipp_target(
        &self,
        db: &rusqlite::Connection,
        device: &str,
        job: &Job,
    ) -> Result<()> {
        if !self.running.load(Ordering::SeqCst) {
            return Err("服务未启动".into());
        }
        let active: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM devices WHERE id=? AND revoked=0)",
                [device],
                |r| r.get(0),
            )
            .map_err(err)?;
        if !active && !(device == ANONYMOUS_DEVICE && !self.authentication_required()) {
            return Err("设备令牌已撤销".into());
        }
        let config: String = db
            .query_row(
                "SELECT config FROM printers WHERE id=?",
                [&job.printer_id],
                |r| r.get(0),
            )
            .map_err(err)?;
        let printer: SharedPrinter = serde_json::from_str(&config).map_err(err)?;
        if printer.queue != job.queue {
            return Err("目标打印队列已变更".into());
        }
        Self::validate_options(&printer, job.copies, &job.sides, job.paper, &job.color_mode)?;
        Ok(())
    }
    pub fn status(&self) -> ServiceStatus {
        let (discovery_active, discovery_error) = {
            let discovery = self.discovery.lock().unwrap();
            (
                discovery.as_ref().is_some_and(|d| d.active()),
                discovery
                    .as_ref()
                    .and_then(|d| d.error())
                    .or_else(|| self.discovery_start_error.lock().unwrap().clone()),
            )
        };
        ServiceStatus {
            running: self.running.load(Ordering::SeqCst),
            simulation: self.simulation,
            address: self.service.lock().unwrap().as_ref().map(|x| x.0.clone()),
            discovery_active,
            discovery_error,
            authentication_required: self
                .configured_authentication_required
                .load(Ordering::SeqCst),
            active_authentication_required: self.authentication_required(),
            authentication_restart_required: self.running.load(Ordering::SeqCst)
                && self.authentication_required()
                    != self
                        .configured_authentication_required
                        .load(Ordering::SeqCst),
        }
    }
    pub fn connection_events(&self) -> Vec<ConnectionEvent> {
        self.connection_events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .cloned()
            .collect()
    }
    pub fn clear_connection_events(&self) -> usize {
        let mut events = self.connection_events.lock().unwrap();
        let count = events.len();
        events.clear();
        count
    }
    pub(crate) fn record_connection_event(&self, event: ConnectionEvent) {
        let mut events = self.connection_events.lock().unwrap();
        if events.len() == 100 {
            events.pop_front();
        }
        events.push_back(event);
    }
    pub fn start(self: &Arc<Self>, addr: &str) -> Result<()> {
        let mut service = self.service.lock().unwrap();
        if service.is_some()
            || self.worker_active.load(Ordering::SeqCst)
            || self.server_active.load(Ordering::SeqCst)
        {
            return Err("服务正在运行或等待打印调用结束".into());
        }
        if !self.simulation && !cfg!(windows) {
            return Err("真实打印仅支持 Windows；本机请使用模拟模式".into());
        }
        if !self.simulation && !self.sumatra.is_file() {
            return Err("请先到设置下载 SumatraPDF 打印组件".into());
        }
        let listener = std::net::TcpListener::bind(addr).map_err(err)?;
        listener.set_nonblocking(true).map_err(err)?;
        let bound_addr = listener.local_addr().map_err(err)?;
        let bound = bound_addr.to_string();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(err)?;
        let authentication_required = self
            .configured_authentication_required
            .load(Ordering::SeqCst);
        self.authentication_required
            .store(authentication_required, Ordering::SeqCst);
        let discovery_result = self.list_printers().and_then(|printers| {
            crate::discovery::Discovery::start(&printers, bound_addr, authentication_required)
        });
        match discovery_result {
            Ok(discovery) => {
                *self.discovery.lock().unwrap() = Some(discovery);
                *self.discovery_start_error.lock().unwrap() = None;
            }
            Err(error) => {
                *self.discovery.lock().unwrap() = None;
                *self.discovery_start_error.lock().unwrap() = Some(error);
            }
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        *service = Some((bound, tx));
        self.running.store(true, Ordering::SeqCst);
        self.server_active.store(true, Ordering::SeqCst);
        let bridge = Arc::clone(self);
        std::thread::spawn(move || {
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("configured listener");
                let _ = axum::serve(
                    listener,
                    crate::service::router(Arc::clone(&bridge))
                        .into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
                bridge.running.store(false, Ordering::SeqCst);
                bridge.discovery.lock().unwrap().take();
                bridge.server_active.store(false, Ordering::SeqCst);
            })
        });
        self.worker_active.store(true, Ordering::SeqCst);
        let bridge = Arc::clone(self);
        std::thread::spawn(move || {
            while bridge.running.load(Ordering::SeqCst) {
                if bridge.process_next().unwrap_or(false) {
                    continue;
                }
                let _ = bridge.cleanup();
                std::thread::sleep(Duration::from_millis(250));
            }
            bridge.worker_active.store(false, Ordering::SeqCst);
        });
        Ok(())
    }
    pub fn stop(&self) -> Result<()> {
        let mut service = self.service.lock().unwrap();
        let _db = self.db.lock().unwrap();
        self.running.store(false, Ordering::SeqCst);
        self.discovery.lock().unwrap().take();
        if let Some((_, tx)) = service.take() {
            let _ = tx.send(());
        }
        Ok(())
    }
    fn process_next(&self) -> Result<bool> {
        let db = self.db.lock().unwrap();
        let printers: Vec<SharedPrinter> = {
            let mut stmt = db.prepare("SELECT config FROM printers").map_err(err)?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(err)?;
            rows.map(|r| serde_json::from_str(&r.map_err(err)?).map_err(err))
                .collect::<Result<Vec<_>>>()?
        };
        if !self.running.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let mut stmt=db.prepare("SELECT id,device_id,printer_id,queue,copies,sides,state,detail,created_at,paper,color_mode,file FROM jobs WHERE state='pending' ORDER BY id").map_err(err)?;
        let mut selected = None;
        for row in stmt
            .query_map([], |r| Ok((job_row(r)?, r.get::<_, String>(11)?)))
            .map_err(err)?
        {
            let (job, file) = row.map_err(err)?;
            if let Some(p) = printers
                .iter()
                .find(|p| p.id == job.printer_id && p.enabled)
            {
                if p.queue != job.queue
                    || Self::validate_options(p, job.copies, &job.sides, job.paper, &job.color_mode)
                        .is_err()
                {
                    db.execute("UPDATE jobs SET state='failed',detail='目标或能力配置已变更',updated_at=? WHERE id=?",params![now(),job.id]).map_err(err)?;
                    continue;
                }
                selected = Some((job, file));
                break;
            }
        }
        drop(stmt);
        let Some((job, file)) = selected else {
            return Ok(false);
        };
        db.execute(
            "UPDATE jobs SET state='processing',updated_at=? WHERE id=? AND state='pending'",
            params![now(), job.id],
        )
        .map_err(err)?;
        drop(db);
        let path = self.root.join("documents").join(file);
        let outcome = if self.simulation {
            if path.is_file() {
                PrintOutcome::Submitted
            } else {
                PrintOutcome::Failed("文档缺失".into())
            }
        } else {
            printing::print_pdf(
                &self.sumatra,
                &job.queue,
                &path,
                &PrintOptions {
                    copies: job.copies,
                    duplex: job.sides,
                    paper: job.paper,
                    color_mode: job.color_mode,
                },
            )
        };
        let (state, detail) = match outcome {
            PrintOutcome::Submitted => (
                if self.simulation {
                    "simulated"
                } else {
                    "submitted"
                },
                if self.simulation {
                    "模拟完成，没有真实打印".to_string()
                } else {
                    "已转交，不代表实际出纸".to_string()
                },
            ),
            PrintOutcome::Failed(s) => ("failed", s),
            PrintOutcome::Uncertain(s) => ("uncertain", s),
        };
        self.db
            .lock()
            .unwrap()
            .execute(
                "UPDATE jobs SET state=?,detail=?,updated_at=? WHERE id=?",
                params![state, detail, now(), job.id],
            )
            .map_err(err)?;
        Ok(true)
    }
    fn cleanup(&self) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE jobs SET state='failed',detail='等待文档超时',updated_at=? WHERE state IN ('pending-document','pending-final-document') AND updated_at < ?",
            params![now(), now() - IPP_DOCUMENT_TIMEOUT_SECS],
        )
        .map_err(err)?;
        let mut stmt=db.prepare("SELECT file FROM jobs WHERE file<>'' AND state IN ('submitted','simulated','failed','canceled','completed') AND updated_at < ?").map_err(err)?;
        for row in stmt
            .query_map([now() - 86400], |r| r.get::<_, String>(0))
            .map_err(err)?
        {
            let path = self.root.join("documents").join(row.map_err(err)?);
            if path.exists() {
                fs::remove_file(path).map_err(err)?;
            }
        }
        Ok(())
    }
}
fn job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        device_id: r.get(1)?,
        printer_id: r.get(2)?,
        queue: r.get(3)?,
        copies: r.get(4)?,
        sides: r.get(5)?,
        state: r.get(6)?,
        detail: r.get(7)?,
        created_at: r.get(8)?,
        paper: {
            let value: String = r.get(9)?;
            PaperSize::parse(&value).ok_or(rusqlite::Error::InvalidQuery)?
        },
        color_mode: r.get(10)?,
    })
}
fn validate_pdf(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_PDF || !bytes.starts_with(b"%PDF-") {
        return Err("无效 PDF 或文档超过 100 MiB".into());
    }
    let doc = lopdf::Document::load_mem(bytes).map_err(|_| "无法解析 PDF".to_string())?;
    if doc.is_encrypted() || doc.trailer.has(b"Encrypt") {
        return Err("不接受加密 PDF".into());
    }
    let count = doc.get_pages().len();
    if count == 0 || count > 500 {
        return Err("PDF 页数须为 1–500".into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lopdf::{dictionary, Document, Object, Stream};
    use std::sync::Barrier;

    pub(crate) fn pdf_bytes() -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let content = document.add_object(Stream::new(dictionary! {}, Vec::new()));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => dictionary! {},
            "Contents" => content,
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
            }),
        );
        let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        document.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        bytes
    }

    pub(crate) fn fixture() -> (tempfile::TempDir, Arc<Bridge>, CreatedDevice) {
        let directory = tempfile::tempdir().unwrap();
        let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        bridge
            .save_printer(SharedPrinter {
                id: "office".into(),
                name: "Office".into(),
                queue: "Queue A".into(),
                enabled: true,
                duplex: true,
                max_copies: 99,
                paper_sizes: default_paper_sizes(),
                default_paper: PaperSize::A4,
            })
            .unwrap();
        let device = bridge.create_device("Mac".into()).unwrap();
        bridge.running.store(true, Ordering::SeqCst);
        (directory, bridge, device)
    }

    #[test]
    fn legacy_database_and_printer_config_keep_a4_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("simulation");
        fs::create_dir_all(&root).unwrap();
        let db = Connection::open(root.join("bridge.sqlite3")).unwrap();
        db.execute_batch("CREATE TABLE printers(id TEXT PRIMARY KEY, config TEXT NOT NULL);
            CREATE TABLE jobs(id INTEGER PRIMARY KEY AUTOINCREMENT,device_id TEXT NOT NULL,printer_id TEXT NOT NULL,queue TEXT NOT NULL,copies INTEGER NOT NULL,sides TEXT NOT NULL,state TEXT NOT NULL,detail TEXT NOT NULL DEFAULT '',created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,file TEXT NOT NULL,digest TEXT NOT NULL);
            INSERT INTO printers VALUES('office','{\"id\":\"office\",\"name\":\"Office\",\"queue\":\"Queue A\",\"enabled\":true,\"duplex\":false,\"max_copies\":99}');
            INSERT INTO jobs VALUES(1,'old-device','office','Queue A',1,'one-sided','submitted','',1,1,'old.pdf','old-digest');").unwrap();
        drop(db);
        for _ in 0..2 {
            let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
            let printer = bridge.printer("office").unwrap();
            assert_eq!(printer.paper_sizes, vec![PaperSize::A4]);
            assert_eq!(printer.default_paper, PaperSize::A4);
            assert_eq!(bridge.job(1).unwrap().paper, PaperSize::A4);
            assert_eq!(bridge.job(1).unwrap().color_mode, "auto");
            assert_eq!(bridge.job(1).unwrap().state, "submitted");
        }
    }

    #[test]
    fn paper_configuration_is_validated_and_jobs_preserve_selected_size() {
        let (directory, bridge, device) = fixture();
        let mut printer = bridge.printer("office").unwrap();
        for sizes in [
            vec![],
            vec![PaperSize::Letter],
            vec![PaperSize::A4, PaperSize::A4],
        ] {
            printer.paper_sizes = sizes;
            assert!(bridge.save_printer(printer.clone()).is_err());
        }
        printer.paper_sizes = vec![PaperSize::A4, PaperSize::Letter];
        printer.default_paper = PaperSize::Letter;
        bridge.save_printer(printer.clone()).unwrap();
        assert!(bridge
            .accept(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A3,
                &pdf_bytes()
            )
            .is_err());
        let job = bridge
            .accept(
                &device.device.id,
                "office",
                2,
                "two-sided-short-edge",
                PaperSize::Letter,
                &pdf_bytes(),
            )
            .unwrap();
        assert_eq!(
            bridge
                .device_jobs(&device.device.id, "office", "all", 20)
                .unwrap()[0]
                .paper,
            PaperSize::Letter
        );
        drop(bridge);
        let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        assert_eq!(bridge.job(job.id).unwrap().paper, PaperSize::Letter);
        assert_eq!(bridge.job(job.id).unwrap().copies, 2);
        assert_eq!(bridge.job(job.id).unwrap().sides, "two-sided-short-edge");
        assert_eq!(
            bridge.printer("office").unwrap().default_paper,
            PaperSize::Letter
        );
        bridge.running.store(true, Ordering::SeqCst);
        printer.paper_sizes = vec![PaperSize::A4];
        printer.default_paper = PaperSize::A4;
        bridge.save_printer(printer).unwrap();
        assert!(!bridge.process_next().unwrap());
        assert_eq!(bridge.job(job.id).unwrap().state, "failed");
    }

    #[test]
    fn print_color_mode_survives_database_reopen_for_print_and_create_job() {
        let (directory, bridge, device) = fixture();
        let printed = bridge
            .accept_format(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A4,
                "color",
                "application/pdf",
                &pdf_bytes(),
            )
            .unwrap();
        let created = bridge
            .create_ipp_job(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A4,
                "monochrome",
            )
            .unwrap();
        assert_eq!(printed.color_mode, "color");
        assert_eq!(created.color_mode, "monochrome");
        drop(bridge);

        let reopened = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        assert_eq!(reopened.job(printed.id).unwrap().color_mode, "color");
        assert_eq!(reopened.job(created.id).unwrap().color_mode, "monochrome");
    }

    fn submit(bridge: &Bridge, device: &CreatedDevice) -> Job {
        bridge
            .accept(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A4,
                &pdf_bytes(),
            )
            .unwrap()
    }

    fn create_waiting_document(bridge: &Bridge, device: &CreatedDevice) -> Job {
        bridge
            .create_ipp_job(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A4,
                "auto",
            )
            .unwrap()
    }

    #[test]
    fn create_job_waits_until_the_last_document_is_received() {
        let (_directory, bridge, device) = fixture();
        let job = create_waiting_document(&bridge, &device);
        assert_eq!(job.state, "pending-document");
        assert!(!bridge.process_next().unwrap());

        let staged = bridge
            .send_ipp_document(
                &device.device.id,
                job.id,
                &pdf_bytes(),
                "application/pdf",
                false,
            )
            .unwrap();
        assert_eq!(staged.state, "pending-final-document");
        assert!(!bridge.process_next().unwrap());

        let ready = bridge
            .send_ipp_document(&device.device.id, job.id, &[], "application/pdf", true)
            .unwrap();
        assert_eq!(ready.state, "pending");
        assert!(bridge.process_next().unwrap());
        assert_eq!(bridge.job(job.id).unwrap().state, "simulated");
    }

    #[test]
    fn pending_document_can_be_canceled_and_empty_job_is_completed_without_printing() {
        let (_directory, bridge, device) = fixture();
        let canceled = create_waiting_document(&bridge, &device);
        bridge.cancel_job(canceled.id).unwrap();
        assert!(bridge
            .send_ipp_document(
                &device.device.id,
                canceled.id,
                &pdf_bytes(),
                "application/pdf",
                true,
            )
            .is_err());
        assert_eq!(bridge.job(canceled.id).unwrap().state, "canceled");

        let empty = create_waiting_document(&bridge, &device);
        let completed = bridge
            .send_ipp_document(&device.device.id, empty.id, &[], "application/pdf", true)
            .unwrap();
        assert_eq!(completed.state, "completed");
        assert!(!bridge.process_next().unwrap());
        let file: String = bridge
            .db
            .lock()
            .unwrap()
            .query_row("SELECT file FROM jobs WHERE id=?", [empty.id], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(file.is_empty());
        assert_eq!(
            bridge
                .device_jobs(&device.device.id, "office", "completed", 20)
                .unwrap()[0]
                .id,
            empty.id
        );
    }

    #[test]
    fn send_document_rechecks_service_device_printer_and_wait_timeout() {
        let (_directory, bridge, device) = fixture();
        let revoked = create_waiting_document(&bridge, &device);
        bridge.revoke_device(&device.device.id).unwrap();
        assert!(bridge
            .send_ipp_document(
                &device.device.id,
                revoked.id,
                &pdf_bytes(),
                "application/pdf",
                true,
            )
            .is_err());
        assert_eq!(bridge.job(revoked.id).unwrap().state, "pending-document");

        let device = bridge.create_device("Replacement Mac".into()).unwrap();
        let stopped = create_waiting_document(&bridge, &device);
        bridge.stop().unwrap();
        assert!(bridge
            .send_ipp_document(
                &device.device.id,
                stopped.id,
                &pdf_bytes(),
                "application/pdf",
                true,
            )
            .is_err());
        assert_eq!(bridge.job(stopped.id).unwrap().state, "pending-document");
        bridge.running.store(true, Ordering::SeqCst);

        let disabled = create_waiting_document(&bridge, &device);
        let mut printer = bridge.printer("office").unwrap();
        printer.enabled = false;
        bridge.save_printer(printer.clone()).unwrap();
        assert!(bridge
            .send_ipp_document(
                &device.device.id,
                disabled.id,
                &pdf_bytes(),
                "application/pdf",
                true,
            )
            .is_err());
        assert_eq!(bridge.job(disabled.id).unwrap().state, "pending-document");
        printer.enabled = true;
        bridge.save_printer(printer).unwrap();

        let timed_out = create_waiting_document(&bridge, &device);
        bridge
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE jobs SET updated_at=? WHERE id=?",
                params![now() - IPP_DOCUMENT_TIMEOUT_SECS - 1, timed_out.id],
            )
            .unwrap();
        assert!(bridge
            .send_ipp_document(
                &device.device.id,
                timed_out.id,
                &[],
                "application/pdf",
                true
            )
            .is_err());
        assert_eq!(bridge.job(timed_out.id).unwrap().state, "failed");
    }

    fn document_path(bridge: &Bridge, id: i64) -> PathBuf {
        let file: String = bridge
            .db
            .lock()
            .unwrap()
            .query_row("SELECT file FROM jobs WHERE id=?", [id], |r| r.get(0))
            .unwrap();
        bridge.root.join("documents").join(file)
    }

    #[test]
    fn accepts_structured_pdf_but_rejects_header_only_and_pageless_documents() {
        assert!(validate_pdf(&pdf_bytes()).is_ok());
        for bytes in [
            b"not a PDF".as_slice(),
            b"%PDF-1.7\ninvalid\n%%EOF".as_slice(),
        ] {
            assert!(validate_pdf(bytes).is_err());
        }
        let mut empty = Document::with_version("1.5");
        let mut bytes = Vec::new();
        empty.save_to(&mut bytes).unwrap();
        assert!(validate_pdf(&bytes).is_err());
    }

    #[test]
    fn authentication_setting_defaults_on_and_persists() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        assert!(bridge.status().authentication_required);
        bridge.set_authentication_required(false).unwrap();
        drop(bridge);
        let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        assert!(!bridge.status().authentication_required);
        bridge.running.store(true, Ordering::SeqCst);
        bridge.set_authentication_required(true).unwrap();
        assert!(bridge.status().authentication_required);
        assert!(!bridge.authentication_required());
        assert!(bridge.status().authentication_restart_required);
        bridge.running.store(false, Ordering::SeqCst);
        bridge.set_authentication_required(true).unwrap();
        assert!(bridge.authentication_required());
    }

    #[test]
    fn opening_store_removes_old_revoked_credentials_and_keeps_active_devices() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        let revoked = bridge.create_device("old Mac".into()).unwrap();
        let active = bridge.create_device("current Mac".into()).unwrap();
        bridge
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE devices SET revoked=1 WHERE id=?",
                [&revoked.device.id],
            )
            .unwrap();
        drop(bridge);
        let bridge = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        assert_eq!(bridge.list_devices().unwrap().len(), 1);
        assert!(bridge.authenticate(&active.device.id, &active.token));
        assert!(!bridge.authenticate(&revoked.device.id, &revoked.token));
        let retained: i64 = bridge
            .db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM devices", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, 1);
    }

    #[test]
    fn connection_history_keeps_latest_100_in_newest_first_order() {
        let (_directory, bridge, _) = fixture();
        for timestamp in 0..105 {
            bridge.record_connection_event(ConnectionEvent {
                timestamp,
                client_ip: None,
                method: "POST".into(),
                path: "/ipp/printers/office".into(),
                operation: Some(crate::ipp::GET_PRINTER_ATTRIBUTES),
                http_status: 200,
                ipp_status: Some(0),
                detail: "Capability query".into(),
            });
        }
        let events = bridge.connection_events();
        assert_eq!(events.len(), 100);
        assert_eq!(events.first().unwrap().timestamp, 104);
        assert_eq!(events.last().unwrap().timestamp, 5);
    }

    #[test]
    fn clearing_connection_history_allows_new_requests() {
        let (_directory, bridge, _) = fixture();
        assert_eq!(bridge.clear_connection_events(), 0);
        let event = ConnectionEvent {
            timestamp: 1,
            client_ip: Some("192.168.1.8".into()),
            method: "POST".into(),
            path: "/ipp/printers/office".into(),
            operation: Some(crate::ipp::GET_PRINTER_ATTRIBUTES),
            http_status: 200,
            ipp_status: Some(0),
            detail: "Capability query".into(),
        };
        bridge.record_connection_event(event.clone());
        bridge.record_connection_event(event.clone());
        assert_eq!(bridge.clear_connection_events(), 2);
        assert!(bridge.connection_events().is_empty());
        bridge.record_connection_event(ConnectionEvent {
            timestamp: 2,
            ..event
        });
        assert_eq!(bridge.connection_events().len(), 1);
        assert_eq!(bridge.connection_events()[0].timestamp, 2);
    }

    #[test]
    fn clearing_job_history_removes_finished_jobs_and_documents_but_keeps_unresolved_jobs() {
        let (directory, bridge, device) = fixture();
        assert_eq!(bridge.clear_job_history().unwrap(), 0);
        let mut removed = Vec::new();
        let mut retained = Vec::new();
        for state in [
            "submitted",
            "simulated",
            "failed",
            "canceled",
            "completed",
            "pending",
            "pending-document",
            "pending-final-document",
            "processing",
            "uncertain",
        ] {
            let job = submit(&bridge, &device);
            bridge
                .db
                .lock()
                .unwrap()
                .execute("UPDATE jobs SET state=? WHERE id=?", params![state, job.id])
                .unwrap();
            let entry = (job.id, document_path(&bridge, job.id));
            if matches!(
                state,
                "submitted" | "simulated" | "failed" | "canceled" | "completed"
            ) {
                removed.push(entry);
            } else {
                retained.push(entry);
            }
        }
        fs::remove_file(&removed[0].1).unwrap();
        let no_document = bridge
            .create_ipp_job(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A4,
                "auto",
            )
            .unwrap();
        bridge.cancel_job(no_document.id).unwrap();
        assert_eq!(bridge.clear_job_history().unwrap(), 6);
        assert!(bridge.job(no_document.id).is_err());
        for (id, path) in &removed {
            assert!(bridge.job(*id).is_err());
            assert!(!path.exists());
        }
        for (id, path) in &retained {
            assert!(bridge.job(*id).is_ok());
            assert!(path.is_file());
        }
        assert_eq!(bridge.list_jobs().unwrap().len(), retained.len());
        assert_eq!(bridge.clear_job_history().unwrap(), 0);
        let next = submit(&bridge, &device);
        assert!(next.id > no_document.id);
        assert_eq!(bridge.list_devices().unwrap().len(), 1);
        assert_eq!(bridge.list_printers().unwrap().len(), 1);
        drop(bridge);
        let reopened = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        assert_eq!(reopened.list_jobs().unwrap().len(), retained.len() + 1);
        for (id, _) in removed {
            assert!(reopened.job(id).is_err());
        }
    }

    #[test]
    fn recovery_never_replays_processing_jobs_and_preserves_pending_jobs() {
        let (directory, bridge, device) = fixture();
        let processing = submit(&bridge, &device);
        let pending = submit(&bridge, &device);
        bridge
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE jobs SET state='processing' WHERE id=?",
                [processing.id],
            )
            .unwrap();
        drop(bridge);
        let recovered = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        assert_eq!(recovered.job(processing.id).unwrap().state, "uncertain");
        assert_eq!(recovered.job(pending.id).unwrap().state, "pending");
        recovered.running.store(true, Ordering::SeqCst);
        assert!(recovered.process_next().unwrap());
        assert_eq!(recovered.job(pending.id).unwrap().state, "simulated");
        assert_eq!(recovered.job(processing.id).unwrap().state, "uncertain");
        assert!(!recovered.process_next().unwrap());
    }

    #[test]
    fn recovery_fails_pending_jobs_with_missing_documents() {
        let (directory, bridge, device) = fixture();
        let job = submit(&bridge, &device);
        fs::remove_file(document_path(&bridge, job.id)).unwrap();
        drop(bridge);
        let recovered = Bridge::open(directory.path(), true, PathBuf::new()).unwrap();
        assert_eq!(recovered.job(job.id).unwrap().state, "failed");
        recovered.running.store(true, Ordering::SeqCst);
        assert!(!recovered.process_next().unwrap());
    }

    #[test]
    fn rejects_duplicate_windows_queue_bindings() {
        let (_directory, bridge, _device) = fixture();
        let mut printer = bridge.printer("office").unwrap();
        printer.id = "warehouse".into();
        printer.queue = "QUEUE a".into();
        assert!(bridge.save_printer(printer).is_err());
        assert_eq!(bridge.list_printers().unwrap().len(), 1);
    }

    #[test]
    fn successful_cancellation_never_executes_even_when_racing_a_worker() {
        let (_directory, bridge, device) = fixture();
        let job = submit(&bridge, &device);
        bridge.cancel_job(job.id).unwrap();
        assert!(!bridge.process_next().unwrap());
        assert_eq!(bridge.job(job.id).unwrap().state, "canceled");
        for _ in 0..32 {
            let job = submit(&bridge, &device);
            let barrier = Arc::new(Barrier::new(2));
            let worker_bridge = Arc::clone(&bridge);
            let worker_barrier = Arc::clone(&barrier);
            let worker = std::thread::spawn(move || {
                worker_barrier.wait();
                worker_bridge.process_next().unwrap()
            });
            barrier.wait();
            let canceled = bridge.cancel_job(job.id).is_ok();
            let executed = worker.join().unwrap();
            let state = bridge.job(job.id).unwrap().state;
            if canceled {
                assert!(!executed);
                assert_eq!(state, "canceled");
            } else {
                assert!(executed);
                assert_eq!(state, "simulated");
            }
        }
    }

    #[test]
    fn disabled_printer_holds_pending_jobs_until_reenabled() {
        let (_directory, bridge, device) = fixture();
        let job = submit(&bridge, &device);
        let mut printer = bridge.printer("office").unwrap();
        printer.enabled = false;
        bridge.save_printer(printer.clone()).unwrap();
        assert!(!bridge.process_next().unwrap());
        assert_eq!(bridge.job(job.id).unwrap().state, "pending");
        printer.enabled = true;
        bridge.save_printer(printer).unwrap();
        assert!(bridge.process_next().unwrap());
        assert_eq!(bridge.job(job.id).unwrap().state, "simulated");
    }

    #[test]
    fn revoked_device_cannot_authenticate_or_submit_but_existing_jobs_remain() {
        let (_directory, bridge, device) = fixture();
        assert!(bridge.authenticate(&device.device.id, &device.token));
        assert!(!bridge.authenticate(&device.device.id, "wrong-token"));
        let job = submit(&bridge, &device);
        bridge.revoke_device(&device.device.id).unwrap();
        assert!(bridge.list_devices().unwrap().is_empty());
        let retained: i64 = bridge
            .db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM devices", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, 0);
        assert!(!bridge.authenticate(&device.device.id, &device.token));
        assert!(bridge
            .accept(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A4,
                &pdf_bytes()
            )
            .is_err());
        assert_eq!(bridge.list_jobs().unwrap().len(), 1);
        assert_eq!(bridge.job(job.id).unwrap().state, "pending");
    }

    #[test]
    fn identical_documents_are_distinct_intentional_submissions() {
        let (_directory, bridge, device) = fixture();
        let pdf = pdf_bytes();
        let first = bridge
            .accept(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A4,
                &pdf,
            )
            .unwrap();
        let second = bridge
            .accept(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A4,
                &pdf,
            )
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_ne!(
            document_path(&bridge, first.id),
            document_path(&bridge, second.id)
        );
        assert!(bridge.process_next().unwrap());
        assert!(bridge.process_next().unwrap());
        assert!(!bridge.process_next().unwrap());
    }

    #[test]
    fn cleanup_preserves_pending_processing_and_uncertain_documents() {
        let (_directory, bridge, device) = fixture();
        let jobs: Vec<_> = (0..4).map(|_| submit(&bridge, &device)).collect();
        for (job, state) in jobs
            .iter()
            .zip(["pending", "processing", "uncertain", "submitted"])
        {
            bridge
                .db
                .lock()
                .unwrap()
                .execute(
                    "UPDATE jobs SET state=?,updated_at=? WHERE id=?",
                    params![state, now() - 90000, job.id],
                )
                .unwrap();
        }
        bridge.cleanup().unwrap();
        for job in &jobs[..3] {
            assert!(document_path(&bridge, job.id).is_file());
        }
        assert!(!document_path(&bridge, jobs[3].id).exists());
    }

    #[test]
    fn active_jobs_prevent_rebinding_and_stopped_service_rejects_submission() {
        let (_directory, bridge, device) = fixture();
        let job = submit(&bridge, &device);
        let mut printer = bridge.printer("office").unwrap();
        printer.queue = "Queue B".into();
        assert!(bridge.save_printer(printer).is_err());
        bridge.stop().unwrap();
        assert!(!bridge.process_next().unwrap());
        assert!(bridge
            .accept(
                &device.device.id,
                "office",
                1,
                "one-sided",
                PaperSize::A4,
                &pdf_bytes()
            )
            .is_err());
        assert_eq!(bridge.job(job.id).unwrap().state, "pending");
    }

    #[test]
    fn device_jobs_filter_before_limiting_history() {
        let (_directory, bridge, device) = fixture();
        let pending = submit(&bridge, &device);
        let canceled = submit(&bridge, &device);
        bridge.cancel_job(canceled.id).unwrap();
        bridge.db.lock().unwrap().execute(
            "WITH RECURSIVE sequence(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM sequence WHERE n<1001)
             INSERT INTO jobs(device_id,printer_id,queue,copies,sides,state,created_at,updated_at,file,digest)
             SELECT 'other-device','office','Queue A',1,'one-sided','canceled',0,0,'','' FROM sequence", [],
        ).unwrap();
        let pending_jobs = bridge
            .device_jobs(&device.device.id, "office", "not-completed", 20)
            .unwrap();
        assert_eq!(pending_jobs.len(), 1);
        assert_eq!(pending_jobs[0].id, pending.id);
        let completed = bridge
            .device_jobs(&device.device.id, "office", "completed", 20)
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].id, canceled.id);
        assert!(bridge
            .device_jobs(&device.device.id, "other-printer", "all", 20)
            .unwrap()
            .is_empty());
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use crate::ipp::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use std::io::{Read, Write};

    #[test]
    fn loopback_chunked_print_is_persisted_and_simulated_then_service_restarts() {
        let (_directory, bridge, device) = tests::fixture();
        bridge.stop().unwrap();
        bridge.start("127.0.0.1:0").unwrap();
        let address = bridge.status().address.unwrap();
        let mut attributes = operation_attributes();
        attributes.attributes.push(Attribute::text(
            URI,
            "printer-uri",
            &format!("ipp://{address}/ipp/printers/office"),
        ));
        attributes.attributes.push(Attribute::text(
            MIME_MEDIA_TYPE,
            "document-format",
            "application/pdf",
        ));
        let body = encode((1, 1), PRINT_JOB, 42, &[attributes], &tests::pdf_bytes()).unwrap();
        let authorization = STANDARD.encode(format!("{}:{}", device.device.id, device.token));
        let mut stream = std::net::TcpStream::connect(&address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(stream,"POST /ipp/printers/office HTTP/1.1\r\nHost: {address}\r\nAuthorization: Basic {authorization}\r\nContent-Type: application/ipp\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
        for chunk in body.chunks(127) {
            write!(stream, "{:x}\r\n", chunk.len()).unwrap();
            stream.write_all(chunk).unwrap();
            stream.write_all(b"\r\n").unwrap();
        }
        stream.write_all(b"0\r\n\r\n").unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200"));
        let offset = response.windows(4).position(|b| b == b"\r\n\r\n").unwrap() + 4;
        let message = parse(&response[offset..]).unwrap();
        assert_eq!(message.code, SUCCESSFUL_OK);
        assert_eq!(message.request_id, 42);
        let events = bridge.connection_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].client_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(events[0].operation, Some(PRINT_JOB));
        assert_eq!(events[0].ipp_status, Some(SUCCESSFUL_OK));
        let id = message.attribute("job-id").unwrap().as_i32().unwrap() as i64;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while bridge.job(id).unwrap().state != "simulated" && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(bridge.job(id).unwrap().state, "simulated");
        bridge.set_authentication_required(false).unwrap();
        assert!(bridge.authentication_required());
        assert!(bridge.status().authentication_restart_required);
        bridge.stop().unwrap();
        while (bridge.worker_active.load(Ordering::SeqCst)
            || bridge.server_active.load(Ordering::SeqCst))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        bridge.start("127.0.0.1:0").unwrap();
        assert!(!bridge.authentication_required());
        assert!(!bridge.status().authentication_restart_required);
        bridge.stop().unwrap();
    }

    #[test]
    fn instance_lock_and_production_simulation_data_are_isolated() {
        let (directory, bridge, _device) = tests::fixture();
        assert!(Bridge::open(directory.path(), true, PathBuf::new()).is_err());
        let production = Bridge::open(directory.path(), false, PathBuf::new()).unwrap();
        assert!(production.list_printers().unwrap().is_empty());
        assert!(!production.status().simulation);
        assert!(bridge.status().simulation);
    }
}
