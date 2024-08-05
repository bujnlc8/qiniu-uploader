//! examples

use qiniu_uploader::{QiniuRegionEnum, QiniuUploader};
use tokio::fs;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let qiniu = QiniuUploader::new(
        "access_key",
        "secret_key",
        "bucket",
        Some(QiniuRegionEnum::Z0),
        false,
    );
    let file = fs::File::open("./Cargo.lock").await?;
    let file_size = file.metadata().await?.len() as usize;
    // 分片上传，支持设置分片大小或上传线程数量
    qiniu
        .clone()
        .part_upload_file(
            "test/Cargo.lock",
            file,
            file_size,
            Some(1024 * 1024 * 50), // 分片大小
            Some(10),               // 上传线程数量
            None,                   // 进度条样式
        )
        .await?;
    // 直传，文件大小应在1GB以内为宜
    let file = fs::File::open("./Cargo.lock").await?;
    qiniu
        .upload_file(
            "test/Cargo.lock.1",
            file,
            mime::APPLICATION_OCTET_STREAM,
            file_size,
            None,
        )
        .await?;
    Ok(())
}
