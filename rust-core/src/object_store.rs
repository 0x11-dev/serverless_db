use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub type ObjectStoreRef = Arc<dyn ObjectStore>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObjectMeta {
    pub key: String,
    pub len: u64,
}

pub trait ObjectStore: Send + Sync {
    fn describe(&self, key: &str) -> String;
    fn exists(&self, key: &str) -> anyhow::Result<bool>;
    fn read_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>>;
    fn len(&self, key: &str) -> anyhow::Result<Option<u64>>;
    fn put_bytes(&self, key: &str, data: &[u8]) -> anyhow::Result<()>;
    fn put_bytes_if_absent(&self, key: &str, data: &[u8]) -> anyhow::Result<bool> {
        if self.exists(key)? {
            return Ok(false);
        }
        self.put_bytes(key, data)?;
        Ok(true)
    }
    fn put_file(&self, key: &str, source: &Path) -> anyhow::Result<()>;
    fn delete(&self, key: &str) -> anyhow::Result<()>;
    fn delete_prefix(&self, prefix: &str) -> anyhow::Result<()>;
    fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>>;
}

#[derive(Clone)]
pub struct AsyncObjectStore {
    inner: ObjectStoreRef,
}

impl AsyncObjectStore {
    pub fn new(inner: ObjectStoreRef) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &ObjectStoreRef {
        &self.inner
    }

    pub async fn read_bytes(&self, key: String) -> anyhow::Result<Vec<u8>> {
        let store = self.inner.clone();
        tokio::task::spawn_blocking(move || store.read_bytes(&key)).await?
    }

    pub async fn exists(&self, key: String) -> anyhow::Result<bool> {
        let store = self.inner.clone();
        tokio::task::spawn_blocking(move || store.exists(&key)).await?
    }

    pub async fn len(&self, key: String) -> anyhow::Result<Option<u64>> {
        let store = self.inner.clone();
        tokio::task::spawn_blocking(move || store.len(&key)).await?
    }

    pub async fn put_bytes(&self, key: String, data: Vec<u8>) -> anyhow::Result<()> {
        let store = self.inner.clone();
        tokio::task::spawn_blocking(move || store.put_bytes(&key, &data)).await?
    }

    pub async fn delete(&self, key: String) -> anyhow::Result<()> {
        let store = self.inner.clone();
        tokio::task::spawn_blocking(move || store.delete(&key)).await?
    }

    pub async fn list_prefix(&self, prefix: String) -> anyhow::Result<Vec<ObjectMeta>> {
        let store = self.inner.clone();
        tokio::task::spawn_blocking(move || store.list_prefix(&prefix)).await?
    }
}

#[derive(Debug, Clone)]
pub struct LocalObjectStore {
    base_dir: PathBuf,
}

impl LocalObjectStore {
    pub fn new(base_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let base_dir = base_dir
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| base_dir.as_ref().to_path_buf());
        fs::create_dir_all(&base_dir)?;
        Ok(Self { base_dir })
    }

    fn path(&self, key: &str) -> anyhow::Result<PathBuf> {
        let mut path = self.base_dir.clone();
        for part in key.split('/') {
            if part.is_empty() || part == "." || part == ".." {
                anyhow::bail!("invalid object key: {key}");
            }
            path.push(part);
        }
        let normalized = normalize_without_existing_check(&path);
        if normalized != self.base_dir && !normalized.starts_with(&self.base_dir) {
            anyhow::bail!("object store path escapes base directory");
        }
        Ok(path)
    }
}

impl ObjectStore for LocalObjectStore {
    fn describe(&self, key: &str) -> String {
        self.path(key)
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|_| format!("local://invalid/{key}"))
    }

    fn exists(&self, key: &str) -> anyhow::Result<bool> {
        Ok(self.path(key)?.exists())
    }

    fn read_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        Ok(fs::read(self.path(key)?)?)
    }

    fn len(&self, key: &str) -> anyhow::Result<Option<u64>> {
        let path = self.path(key)?;
        Ok(fs::metadata(path).map(|meta| meta.len()).ok())
    }

    fn put_bytes(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
        let dest = self.path(key)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_file_name(format!(
            ".{}.tmp-{}",
            dest.file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("object"),
            unique_temp_suffix()
        ));
        {
            let mut file = File::create(&tmp)?;
            file.write_all(data)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &dest)?;
        Ok(())
    }

    fn put_bytes_if_absent(&self, key: &str, data: &[u8]) -> anyhow::Result<bool> {
        let dest = self.path(key)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_file_name(format!(
            ".{}.create-if-absent-{}-{}",
            dest.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("object"),
            std::process::id(),
            unique_temp_suffix()
        ));
        {
            let mut file = File::create(&tmp)?;
            file.write_all(data)?;
            file.sync_all()?;
        }
        match fs::hard_link(&tmp, &dest) {
            Ok(()) => {
                fs::remove_file(&tmp).ok();
                Ok(true)
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                fs::remove_file(&tmp).ok();
                Ok(false)
            }
            Err(err) => {
                fs::remove_file(&tmp).ok();
                Err(err.into())
            }
        }
    }

    fn put_file(&self, key: &str, source: &Path) -> anyhow::Result<()> {
        let dest = self.path(key)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_file_name(format!(
            ".{}.tmp-{}",
            dest.file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("object"),
            unique_temp_suffix()
        ));
        fs::copy(source, &tmp)?;
        File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, &dest)?;
        Ok(())
    }

    fn delete(&self, key: &str) -> anyhow::Result<()> {
        let path = self.path(key)?;
        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn delete_prefix(&self, prefix: &str) -> anyhow::Result<()> {
        let path = self.path(prefix.trim_end_matches('/'))?;
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
        let prefix = prefix.trim_end_matches('/');
        let root = self.path(prefix)?;
        let mut out = Vec::new();
        if !root.exists() {
            return Ok(out);
        }
        collect_local_objects(&root, prefix, &mut out)?;
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

fn collect_local_objects(
    path: &Path,
    prefix: &str,
    out: &mut Vec<ObjectMeta>,
) -> anyhow::Result<()> {
    if path.is_file() {
        out.push(ObjectMeta {
            key: prefix.to_string(),
            len: fs::metadata(path)?.len(),
        });
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let key = format!("{}/{}", prefix.trim_end_matches('/'), name);
        let path = entry.path();
        if path.is_dir() {
            collect_local_objects(&path, &key, out)?;
        } else {
            out.push(ObjectMeta {
                key,
                len: entry.metadata()?.len(),
            });
        }
    }
    Ok(())
}

fn normalize_without_existing_check(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn unique_temp_suffix() -> String {
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);
    format!(
        "{}-{}-{}",
        std::process::id(),
        now_nanos(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::SeqCst)
    )
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(feature = "s3")]
pub mod s3 {
    use super::{ObjectMeta, ObjectStore};
    use aws_config::{BehaviorVersion, Region};
    use aws_sdk_s3::{Client, config::Builder as S3ConfigBuilder, primitives::ByteStream};
    use std::future::Future;
    use std::path::Path;
    use std::time::Duration;
    use tokio::runtime::Runtime;

    pub struct S3ObjectStore {
        runtime: Runtime,
        client: Client,
        bucket: String,
        prefix: String,
    }

    impl S3ObjectStore {
        pub fn new(config: S3ObjectStoreConfig) -> anyhow::Result<Self> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let region = config.region.clone();
            let endpoint = config.endpoint.clone();
            let force_path_style = config.force_path_style;
            let client = block_on_aws(&runtime, async move {
                let mut loader = aws_config::defaults(BehaviorVersion::latest());
                if let Some(region) = region {
                    loader = loader.region(Region::new(region));
                }
                if let Some(endpoint) = endpoint {
                    loader = loader.endpoint_url(endpoint);
                }
                let shared = loader.load().await;
                let mut builder = S3ConfigBuilder::from(&shared);
                if force_path_style {
                    builder = builder.force_path_style(true);
                }
                Client::from_conf(builder.build())
            });
            Ok(Self {
                runtime,
                client,
                bucket: config.bucket,
                prefix: config.prefix.trim_matches('/').to_string(),
            })
        }

        fn key(&self, key: &str) -> String {
            if self.prefix.is_empty() {
                key.to_string()
            } else {
                format!("{}/{}", self.prefix, key)
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct S3ObjectStoreConfig {
        pub bucket: String,
        pub prefix: String,
        pub endpoint: Option<String>,
        pub region: Option<String>,
        pub force_path_style: bool,
    }

    impl ObjectStore for S3ObjectStore {
        fn describe(&self, key: &str) -> String {
            format!("s3://{}/{}", self.bucket, self.key(key))
        }

        fn exists(&self, key: &str) -> anyhow::Result<bool> {
            Ok(self.len(key)?.is_some())
        }

        fn read_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>> {
            let key = self.key(key);
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            block_on_aws(
                &self.runtime,
                retry_async(move || {
                    let client = client.clone();
                    let bucket = bucket.clone();
                    let key = key.clone();
                    async move {
                        let object = client.get_object().bucket(bucket).key(key).send().await?;
                        let bytes = object.body.collect().await?.into_bytes();
                        Ok(bytes.to_vec())
                    }
                }),
            )
        }

        fn len(&self, key: &str) -> anyhow::Result<Option<u64>> {
            let key = self.key(key);
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            block_on_aws(
                &self.runtime,
                retry_async(move || {
                    let client = client.clone();
                    let bucket = bucket.clone();
                    let key = key.clone();
                    async move {
                        match client.head_object().bucket(bucket).key(key).send().await {
                            Ok(head) => Ok(head.content_length().map(|len| len as u64)),
                            Err(err)
                                if err.as_service_error().is_some_and(|err| err.is_not_found()) =>
                            {
                                Ok(None)
                            }
                            Err(err) => Err(anyhow::Error::new(err)),
                        }
                    }
                }),
            )
        }

        fn put_bytes(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
            let key = self.key(key);
            let data = data.to_vec();
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            block_on_aws(
                &self.runtime,
                retry_async(move || {
                    let client = client.clone();
                    let bucket = bucket.clone();
                    let key = key.clone();
                    let data = data.clone();
                    async move {
                        client
                            .put_object()
                            .bucket(bucket)
                            .key(key)
                            .body(ByteStream::from(data))
                            .send()
                            .await?;
                        Ok(())
                    }
                }),
            )
        }

        fn put_bytes_if_absent(&self, key: &str, data: &[u8]) -> anyhow::Result<bool> {
            let key = self.key(key);
            let data = data.to_vec();
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            block_on_aws(&self.runtime, async move {
                let debug_bucket = bucket.clone();
                let debug_key = key.clone();
                let response = client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .if_none_match("*")
                    .body(ByteStream::from(data))
                    .send()
                    .await;
                match response {
                    Ok(_) => Ok(true),
                    Err(err) if is_precondition_failed(&err) => Ok(false),
                    Err(err) => Err(anyhow::anyhow!(
                        "S3 put_bytes_if_absent failed for {debug_bucket}/{debug_key}: {err:?}"
                    )),
                }
            })
        }

        fn put_file(&self, key: &str, source: &Path) -> anyhow::Result<()> {
            self.put_bytes(key, &std::fs::read(source)?)
        }

        fn delete(&self, key: &str) -> anyhow::Result<()> {
            let key = self.key(key);
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            block_on_aws(
                &self.runtime,
                retry_async(move || {
                    let client = client.clone();
                    let bucket = bucket.clone();
                    let key = key.clone();
                    async move {
                        client
                            .delete_object()
                            .bucket(bucket)
                            .key(key)
                            .send()
                            .await?;
                        Ok(())
                    }
                }),
            )
        }

        fn delete_prefix(&self, prefix: &str) -> anyhow::Result<()> {
            let objects = self.list_prefix(prefix)?;
            for object in objects {
                self.delete(&object.key)?;
            }
            Ok(())
        }

        fn list_prefix(&self, prefix: &str) -> anyhow::Result<Vec<ObjectMeta>> {
            let prefix = self.key(prefix.trim_end_matches('/'));
            let strip_prefix = self.prefix.clone();
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            block_on_aws(
                &self.runtime,
                retry_async(move || {
                    let client = client.clone();
                    let bucket = bucket.clone();
                    let prefix = prefix.clone();
                    let strip_prefix = strip_prefix.clone();
                    async move {
                        let mut out = Vec::new();
                        let mut continuation_token = None;
                        loop {
                            let mut request =
                                client.list_objects_v2().bucket(&bucket).prefix(&prefix);
                            if let Some(token) = continuation_token {
                                request = request.continuation_token(token);
                            }
                            let response = request.send().await?;
                            for object in response.contents() {
                                if let (Some(key), Some(size)) = (object.key(), object.size()) {
                                    out.push(ObjectMeta {
                                        key: strip_key_prefix(&strip_prefix, key),
                                        len: size as u64,
                                    });
                                }
                            }
                            if response.is_truncated().unwrap_or(false) {
                                continuation_token =
                                    response.next_continuation_token().map(ToOwned::to_owned);
                            } else {
                                break;
                            }
                        }
                        out.sort_by(|a, b| a.key.cmp(&b.key));
                        Ok(out)
                    }
                }),
            )
        }
    }

    fn block_on_aws<F, T>(runtime: &Runtime, future: F) -> T
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| runtime.block_on(future))
            }
            Ok(_) => std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build S3 helper runtime")
                    .block_on(future)
            })
            .join()
            .expect("S3 helper runtime thread panicked"),
            Err(_) => runtime.block_on(future),
        }
    }

    async fn retry_async<T, F, Fut>(mut operation: F) -> anyhow::Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let max_attempts = std::env::var("SDB_S3_ADAPTER_MAX_ATTEMPTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(3)
            .max(1);
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            match operation().await {
                Ok(value) => return Ok(value),
                Err(err) => {
                    last_error = Some(err);
                    if attempt < max_attempts {
                        tokio::time::sleep(Duration::from_millis(25 * attempt as u64)).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("S3 operation failed")))
    }

    fn strip_key_prefix(prefix: &str, key: &str) -> String {
        if prefix.is_empty() {
            key.to_string()
        } else {
            key.trim_start_matches(prefix)
                .trim_start_matches('/')
                .to_string()
        }
    }

    fn is_precondition_failed<E: std::fmt::Debug + std::fmt::Display>(err: &E) -> bool {
        let rendered = format!("{err} {err:?}").to_ascii_lowercase();
        rendered.contains("preconditionfailed")
            || rendered.contains("pre-condition")
            || rendered.contains("statuscode(412)")
            || rendered.contains("status: 412")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn async_object_store_put_read_delete() {
        let dir = tempfile::tempdir().unwrap();
        let local: ObjectStoreRef = Arc::new(LocalObjectStore::new(dir.path()).unwrap());
        let async_store = AsyncObjectStore::new(local);

        async_store
            .put_bytes("foo/bar.bin".to_string(), vec![1, 2, 3, 4])
            .await
            .unwrap();

        assert!(async_store.exists("foo/bar.bin".to_string()).await.unwrap());
        assert!(!async_store.exists("foo/missing".to_string()).await.unwrap());

        let data = async_store
            .read_bytes("foo/bar.bin".to_string())
            .await
            .unwrap();
        assert_eq!(data, vec![1, 2, 3, 4]);

        let len = async_store.len("foo/bar.bin".to_string()).await.unwrap();
        assert_eq!(len, Some(4));

        async_store
            .delete("foo/bar.bin".to_string())
            .await
            .unwrap();
        assert!(!async_store.exists("foo/bar.bin".to_string()).await.unwrap());
    }

    #[tokio::test]
    async fn async_object_store_list_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let local: ObjectStoreRef = Arc::new(LocalObjectStore::new(dir.path()).unwrap());
        let async_store = AsyncObjectStore::new(local);

        async_store
            .put_bytes("a/1.bin".to_string(), vec![1])
            .await
            .unwrap();
        async_store
            .put_bytes("a/2.bin".to_string(), vec![2])
            .await
            .unwrap();
        async_store
            .put_bytes("b/3.bin".to_string(), vec![3])
            .await
            .unwrap();

        let objects = async_store
            .list_prefix("a".to_string())
            .await
            .unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].key, "a/1.bin");
        assert_eq!(objects[1].key, "a/2.bin");
    }
}
