use crate::printing::{self, PrintOptions, PrintOutcome};
use fs2::FileExt;
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
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
    pub state: String,
    pub detail: String,
    pub created_at: i64,
}
#[derive(Serialize)]
pub struct ServiceStatus {
    pub running: bool,
    pub simulation: bool,
    pub address: Option<String>,
}
pub struct Bridge {
    pub(crate) db: Mutex<Connection>,
    root: PathBuf,
    simulation: bool,
    sumatra: PathBuf,
    pub(crate) running: AtomicBool,
    service: Mutex<Option<(String, tokio::sync::oneshot::Sender<()>)>>,
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
            CREATE TABLE IF NOT EXISTS jobs(id INTEGER PRIMARY KEY AUTOINCREMENT,device_id TEXT NOT NULL,printer_id TEXT NOT NULL,queue TEXT NOT NULL,copies INTEGER NOT NULL,sides TEXT NOT NULL,state TEXT NOT NULL,detail TEXT NOT NULL DEFAULT '',created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,file TEXT NOT NULL,digest TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state,id);").map_err(err)?;
        db.execute("UPDATE jobs SET state='uncertain',detail='应用退出前正在调用打印程序，请检查 Windows 队列及出纸情况',updated_at=? WHERE state='processing'", [now()]).map_err(err)?;
        let bridge = Arc::new(Self {
            db: Mutex::new(db),
            root,
            simulation,
            sumatra,
            running: AtomicBool::new(false),
            service: Mutex::new(None),
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
            if state == "pending" && !self.root.join("documents").join(&file).is_file() {
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
            || !(1..=99).contains(&p.max_copies)
        {
            return Err("打印机配置无效：ID 仅允许字母、数字、横线和下划线；份数为 1–99".into());
        }
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT config FROM printers").map_err(err)?;
        for row in stmt.query_map([], |r| r.get::<_, String>(0)).map_err(err)? {
            let old: SharedPrinter = serde_json::from_str(&row.map_err(err)?).map_err(err)?;
            if old.id != p.id && old.queue.to_lowercase() == p.queue.to_lowercase() {
                return Err("此 Windows 队列已有共享绑定".into());
            }
            if old.id == p.id && old.queue != p.queue {
                let n:i64=db.query_row("SELECT COUNT(*) FROM jobs WHERE printer_id=? AND state IN ('pending','processing','uncertain')",[&p.id],|r|r.get(0)).map_err(err)?;
                if n > 0 {
                    return Err("请先处理原绑定的未完成任务".into());
                }
            }
        }
        db.execute("INSERT INTO printers(id,config) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET config=excluded.config",params![p.id,serde_json::to_string(&p).map_err(err)?]).map_err(err)?;
        Ok(())
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
            .prepare("SELECT id,name,revoked FROM devices ORDER BY name")
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
            .execute("UPDATE devices SET revoked=1 WHERE id=?", [id])
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
    pub fn list_jobs(&self) -> Result<Vec<Job>> {
        let db = self.db.lock().unwrap();
        let mut stmt=db.prepare("SELECT id,device_id,printer_id,queue,copies,sides,state,detail,created_at FROM jobs ORDER BY id DESC LIMIT 1000").map_err(err)?;
        let result = stmt
            .query_map([], job_row)
            .map_err(err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(err);
        result
    }
    pub(crate) fn job(&self, id: i64) -> Result<Job> {
        let db = self.db.lock().unwrap();
        db.query_row("SELECT id,device_id,printer_id,queue,copies,sides,state,detail,created_at FROM jobs WHERE id=?",[id],job_row).map_err(|_|"任务不存在".into())
    }
    pub(crate) fn device_jobs(
        &self,
        device: &str,
        printer: &str,
        which: &str,
        limit: i32,
    ) -> Result<Vec<Job>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT id,device_id,printer_id,queue,copies,sides,state,detail,created_at FROM jobs
            WHERE device_id=? AND printer_id=? AND (?='all' OR
                CASE WHEN state IN ('submitted','simulated','failed','canceled') THEN 'completed' ELSE 'not-completed' END=?)
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
        let n=self.db.lock().unwrap().execute("UPDATE jobs SET state='canceled',detail='执行前取消',updated_at=? WHERE id=? AND state='pending'",params![now(),id]).map_err(err)?;
        if n == 1 {
            Ok(())
        } else {
            Err("仅可取消等待中的任务".into())
        }
    }
    pub(crate) fn validate_options(p: &SharedPrinter, copies: u32, sides: &str) -> Result<()> {
        if !p.enabled {
            return Err("打印机已禁用".into());
        }
        if copies == 0 || copies > p.max_copies {
            return Err("不支持的份数".into());
        }
        if sides != "one-sided"
            && (!p.duplex || !matches!(sides, "two-sided-long-edge" | "two-sided-short-edge"))
        {
            return Err("不支持的单双面选项".into());
        }
        Ok(())
    }
    pub(crate) fn accept(
        &self,
        device: &str,
        printer: &str,
        copies: u32,
        sides: &str,
        pdf: &[u8],
    ) -> Result<Job> {
        validate_pdf(pdf)?;
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
        if !active {
            return Err("设备令牌已撤销".into());
        }
        let config: String = tx
            .query_row("SELECT config FROM printers WHERE id=?", [printer], |r| {
                r.get(0)
            })
            .map_err(err)?;
        let p: SharedPrinter = serde_json::from_str(&config).map_err(err)?;
        Self::validate_options(&p, copies, sides)?;
        let count:i64=tx.query_row("SELECT COUNT(*) FROM jobs WHERE state IN ('pending','processing','uncertain') AND device_id=?",[device],|r|r.get(0)).map_err(err)?;
        let queue_count:i64=tx.query_row("SELECT COUNT(*) FROM jobs WHERE state IN ('pending','processing','uncertain') AND printer_id=?",[printer],|r|r.get(0)).map_err(err)?;
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
        let inserted=tx.execute("INSERT INTO jobs(device_id,printer_id,queue,copies,sides,state,created_at,updated_at,file,digest) VALUES(?,?,?,?,?,'pending',?,?,?,?)",params![device,printer,p.queue,copies,sides,now(),now(),name,hash(pdf)]);
        if let Err(e) = inserted {
            let _ = fs::remove_file(&path);
            return Err(err(e));
        }
        let id = tx.last_insert_rowid();
        tx.commit().map_err(err)?;
        drop(db);
        self.job(id)
    }
    pub fn status(&self) -> ServiceStatus {
        ServiceStatus {
            running: self.running.load(Ordering::SeqCst),
            simulation: self.simulation,
            address: self.service.lock().unwrap().as_ref().map(|x| x.0.clone()),
        }
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
            return Err("缺少随应用分发的 SumatraPDF 资源".into());
        }
        let listener = std::net::TcpListener::bind(addr).map_err(err)?;
        listener.set_nonblocking(true).map_err(err)?;
        let bound = listener.local_addr().map_err(err)?.to_string();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(err)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        *service = Some((bound, tx));
        self.running.store(true, Ordering::SeqCst);
        self.server_active.store(true, Ordering::SeqCst);
        let bridge = Arc::clone(self);
        std::thread::spawn(move || {
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("configured listener");
                let _ = axum::serve(listener, crate::service::router(Arc::clone(&bridge)))
                    .with_graceful_shutdown(async {
                        let _ = rx.await;
                    })
                    .await;
                bridge.running.store(false, Ordering::SeqCst);
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
        let mut stmt=db.prepare("SELECT id,device_id,printer_id,queue,copies,sides,state,detail,created_at,file FROM jobs WHERE state='pending' ORDER BY id").map_err(err)?;
        let mut selected = None;
        for row in stmt
            .query_map([], |r| Ok((job_row(r)?, r.get::<_, String>(9)?)))
            .map_err(err)?
        {
            let (job, file) = row.map_err(err)?;
            if let Some(p) = printers
                .iter()
                .find(|p| p.id == job.printer_id && p.enabled)
            {
                if p.queue != job.queue
                    || Self::validate_options(p, job.copies, &job.sides).is_err()
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
        let mut stmt=db.prepare("SELECT file FROM jobs WHERE state IN ('submitted','simulated','failed','canceled') AND updated_at < ?").map_err(err)?;
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
            })
            .unwrap();
        let device = bridge.create_device("Mac".into()).unwrap();
        bridge.running.store(true, Ordering::SeqCst);
        (directory, bridge, device)
    }

    fn submit(bridge: &Bridge, device: &CreatedDevice) -> Job {
        bridge
            .accept(&device.device.id, "office", 1, "one-sided", &pdf_bytes())
            .unwrap()
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
        assert!(!bridge.authenticate(&device.device.id, &device.token));
        assert!(bridge
            .accept(&device.device.id, "office", 1, "one-sided", &pdf_bytes())
            .is_err());
        assert_eq!(bridge.list_jobs().unwrap().len(), 1);
        assert_eq!(bridge.job(job.id).unwrap().state, "pending");
    }

    #[test]
    fn identical_documents_are_distinct_intentional_submissions() {
        let (_directory, bridge, device) = fixture();
        let pdf = pdf_bytes();
        let first = bridge
            .accept(&device.device.id, "office", 1, "one-sided", &pdf)
            .unwrap();
        let second = bridge
            .accept(&device.device.id, "office", 1, "one-sided", &pdf)
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
            .accept(&device.device.id, "office", 1, "one-sided", &pdf_bytes())
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
        let id = message.attribute("job-id").unwrap().as_i32().unwrap() as i64;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while bridge.job(id).unwrap().state != "simulated" && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(bridge.job(id).unwrap().state, "simulated");
        bridge.stop().unwrap();
        while (bridge.worker_active.load(Ordering::SeqCst)
            || bridge.server_active.load(Ordering::SeqCst))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        bridge.start("127.0.0.1:0").unwrap();
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
