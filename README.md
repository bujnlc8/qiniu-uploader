# 七牛文件上传

封装了七牛[直传文件](https://developer.qiniu.com/kodo/1312/upload)和[分片上传 v2 版](https://developer.qiniu.com/kodo/6364/multipartupload-interface)，支持显示上传进度条，由[indicatif](https://crates.io/crates/indicatif)提供支持.

## 使用

```
cargo add qiniu-uploader
```

```rust
use tokio::fs;
use qiniu_uploader::{QiniuUploader, QiniuRegionEnum};

#[tokio::main]
async fn main()->Result<(), anyhow::Error>{
    let qiniu = QiniuUploader::new(
        String::from("access_key"),
        String::from("access_secret"),
        String::from("bucket"),
        Some(QiniuRegionEnum::Z0),
        true,
    );
    let mut f = fs::File::open("./Cargo.lock").await?;
    let file_size = f.metadata().await?.size();
    qiniu
        .part_upload_file_with_progress(
            "test/Cargo.lock",
            f,
            file_size as i64,
            Some(1024 * 1024 * 50),
            Some(10),
            None,
        )
        .await?;
    OK(())
}
```

更详细的参数见[https://docs.rs/qiniu-uploader/](https://docs.rs/qiniu-uploader/)
