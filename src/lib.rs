#![allow(dead_code)]
pub use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::Cursor, str::FromStr};

use base64::prelude::*;
use futures_util::TryStreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use mime::{Mime, APPLICATION_JSON, APPLICATION_OCTET_STREAM};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE},
    multipart::{self, Form},
    Body,
};
use tokio::io::{AsyncReadExt, BufReader};
use tokio_util::io::ReaderStream;

pub use indicatif;
pub use mime;

/// Qiniu上传实例
#[derive(Debug, Clone)]
pub struct QiniuUploader {
    access_key: String,
    secret_key: String,
    bucket: String,
    region: QiniuRegionEnum,
    debug: bool,
}

/// 七牛区域enum，见 <https://developer.qiniu.com/kodo/1671/region-endpoint-fq>
#[derive(Debug, Clone, Copy)]
pub enum QiniuRegionEnum {
    Z0,
    CNEast2,
    Z1,
    Z2,
    NA0,
    AS0,
    APSouthEast2,
    APSouthEast3,
}

impl QiniuRegionEnum {
    pub fn get_upload_host(&self) -> String {
        match self {
            Self::Z0 => String::from("https://up-z0.qiniup.com"),
            Self::Z1 => String::from("https://up-z1.qiniup.com"),
            Self::Z2 => String::from("https://up-z2.qiniup.com"),
            Self::NA0 => String::from("https://up-na0.qiniup.com"),
            Self::AS0 => String::from("https://up-as0.qiniup.com"),
            Self::APSouthEast2 => String::from("https://up-ap-southeast-2.qiniup.com"),
            Self::APSouthEast3 => String::from("https://up-ap-southeast-3.qiniup.com"),
            Self::CNEast2 => String::from("https://up-cn-east-2.qiniup.com"),
        }
    }
}

impl FromStr for QiniuRegionEnum {
    type Err = anyhow::Error;

    fn from_str(region: &str) -> Result<Self, Self::Err> {
        let region = match region {
            "z0" => Self::Z0,
            "cn-east-2" => Self::CNEast2,
            "z1" => Self::Z1,
            "z2" => Self::Z2,
            "na0" => Self::NA0,
            "as0" => Self::AS0,
            "ap-southeast-2" => Self::APSouthEast2,
            "ap-southeast-3" => Self::APSouthEast3,
            _ => return Err(anyhow!("Unknow region: {}", region)),
        };
        Ok(region)
    }
}

/// 初始化分片任务响应
#[derive(Debug, Deserialize)]
struct InitialPartUploadResponse {
    #[serde(rename = "uploadId")]
    pub upload_id: String,

    #[serde(rename = "expireAt")]
    pub expire_at: i64,
}

/// 分片上传响应
#[derive(Debug, Deserialize)]
struct PartUploadResponse {
    pub etag: String,
    pub md5: String,
}

/// 完成分片上传参数
#[derive(Debug, Serialize)]
struct CompletePartUploadParam {
    pub etag: String,

    #[serde(rename = "partNumber")]
    pub part_number: i64,
}

impl QiniuUploader {
    /// # 生成上传实例
    /// ## 参数
    /// - access_key 七牛access_key
    /// - secret_key 七牛secret_key
    /// - bucket 七牛bucket
    /// - region 七牛上传区域，默认z0
    /// - debug 是否开启debug
    pub fn new(
        access_key: String,
        secret_key: String,
        bucket: String,
        region: Option<QiniuRegionEnum>,
        debug: bool,
    ) -> Self {
        let region = region.unwrap_or(QiniuRegionEnum::Z0);
        Self {
            access_key,
            secret_key,
            bucket,
            region,
            debug,
        }
    }

    fn get_upload_token(&self, key: &str) -> String {
        let deadline = chrono::Local::now().timestamp() + 3600;
        let put_policy = r#"{"scope": "{bucket}:{key}", "deadline": {deadline}, "fsizeLimit": 1073741824, "returnBody": "{\"hash\": $(etag), \"key\": $(key)}"}"#;
        let put_policy = put_policy
            .replace("{bucket}", &self.bucket)
            .replace("{deadline}", &deadline.to_string())
            .replace("{key}", key);
        let mut buf = String::new();
        BASE64_URL_SAFE.encode_string(put_policy, &mut buf);
        let hmac_digest = hmac_sha1::hmac_sha1(self.secret_key.as_bytes(), buf.as_bytes());
        let mut sign = String::new();
        BASE64_URL_SAFE.encode_string(hmac_digest, &mut sign);
        let token = format!("{}:{sign}:{buf}", self.access_key);
        if self.debug {
            println!("key: {}, token: {}", key, token);
        }
        token
    }

    /// 直传文件组装multi_part <https://developer.qiniu.com/kodo/1312/upload>
    fn make_multi_part<T: Into<Body>>(&self, key: &str, body: T, mime: Mime) -> Form {
        let token = self.get_upload_token(key);
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_str("Content-Type").unwrap(),
            HeaderValue::from_str(mime.essence_str()).unwrap(),
        );
        let file_name = key.split("/").last().unwrap().to_string();
        multipart::Form::new()
            .part(
                "file",
                multipart::Part::stream(body)
                    .file_name(file_name.clone())
                    .headers(headers)
                    .mime_str(mime.essence_str())
                    .unwrap(),
            )
            .text("key", key.to_string())
            .text("token", token)
            .text("filename", file_name)
    }

    /// # 直传文件，带进度条
    /// <https://developer.qiniu.com/kodo/1312/upload>
    /// ## 参数
    /// - key: 上传文件的key，如test/Cargo.lock
    /// - data: R: AsyncReadExt + Unpin + Send + Sync + 'static
    /// - mime: 文件类型
    /// - 文件大小，单位 bytes
    pub async fn upload_file_with_progress<R: AsyncReadExt + Unpin + Send + Sync + 'static>(
        &self,
        key: &str,
        data: R,
        mime: Mime,
        file_size: usize,
    ) -> Result<(), anyhow::Error> {
        let reader = ReaderStream::new(data);
        let pb = ProgressBar::new(file_size as u64);
        pb.set_style(ProgressStyle::default_bar().template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})").unwrap().progress_chars("#>-"));
        let pb1 = pb.clone();
        let stream = reader.inspect_ok(move |chunk| {
            pb1.inc(chunk.len() as u64);
        });
        let body = Body::wrap_stream(stream);
        let form = self.make_multi_part(key, body, mime);
        if self.debug {
            println!("multi_part: {:#?}", form);
        }
        let response = reqwest::Client::new()
            .post(self.region.get_upload_host())
            .multipart(form)
            .send()
            .await?;
        pb.finish();
        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to upload file: {}, {}",
                response.status().as_u16(),
                response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string())
            ));
        }
        if self.debug {
            println!("upload_file_with_progress response: {:#?}", response);
        }
        Ok(())
    }

    /// # 直传文件
    /// <https://developer.qiniu.com/kodo/1312/upload>
    /// ## 参数
    /// - key: 上传文件的key，如test/Cargo.lock
    /// - data: R: AsyncReadExt + Unpin + Send + Sync + 'static
    /// - mime: 文件类型
    pub async fn upload_file<R: AsyncReadExt + Unpin + Send + Sync + 'static>(
        &self,
        key: &str,
        data: R,
        mime: Mime,
    ) -> Result<(), anyhow::Error> {
        let reader = ReaderStream::new(data);
        let body = Body::wrap_stream(reader);
        let form = self.make_multi_part(key, body, mime);
        let response = reqwest::Client::new()
            .post(self.region.get_upload_host())
            .multipart(form)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to upload file: {} {}",
                response.status().as_u16(),
                response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string())
            ));
        }
        if self.debug {
            println!("upload_file response: {:#?}", response);
        }
        Ok(())
    }

    fn get_part_upload_token(&self, key: &str) -> String {
        format!("UpToken {}", self.get_upload_token(key))
    }

    fn get_base64encode_key(&self, key: &str) -> String {
        let mut res = String::new();
        BASE64_URL_SAFE.encode_string(key, &mut res);
        res
    }

    fn get_part_headers(&self, key: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&self.get_part_upload_token(key)).unwrap(),
        );
        headers
    }

    /// 初始化任务 <https://developer.qiniu.com/kodo/6365/initialize-multipartupload>
    async fn initial_part_upload(
        &self,
        key: &str,
    ) -> Result<InitialPartUploadResponse, anyhow::Error> {
        let url = format!(
            "{}/buckets/{}/objects/{}/uploads",
            self.region.get_upload_host(),
            self.bucket,
            self.get_base64encode_key(key),
        );
        let headers = self.get_part_headers(key);
        let response = reqwest::Client::new()
            .post(url)
            .headers(headers)
            .send()
            .await?
            .json::<InitialPartUploadResponse>()
            .await?;
        if self.debug {
            println!("initial_part_upload response: {:#?}", response);
        }
        Ok(response)
    }

    /// 分块上传数据 <https://developer.qiniu.com/kodo/6366/upload-part>
    async fn part_upload(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        data: Vec<u8>,
        pb: ProgressBar,
    ) -> Result<PartUploadResponse, anyhow::Error> {
        let url = format!(
            "{}/buckets/{}/objects/{}/uploads/{upload_id}/{part_number}",
            self.region.get_upload_host(),
            self.bucket,
            self.get_base64encode_key(key),
        );
        let mut headers = self.get_part_headers(key);
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(APPLICATION_OCTET_STREAM.essence_str()).unwrap(),
        );
        let size = data.len();
        headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&size.to_string()).unwrap(),
        );
        let reader = ReaderStream::new(BufReader::new(Cursor::new(data)));
        let pb1 = pb.clone();
        let stream = reader.inspect_ok(move |chunk| {
            pb1.inc(chunk.len() as u64);
        });
        let body = Body::wrap_stream(stream);
        let response = reqwest::Client::new()
            .put(url)
            .headers(headers)
            .body(body)
            .send()
            .await?
            .json::<PartUploadResponse>()
            .await?;
        if self.debug {
            println!("part_upload response: {:#?}", response);
        }
        pb.finish();
        Ok(response)
    }

    /// 完成文件上传 <https://developer.qiniu.com/kodo/6368/complete-multipart-upload>
    async fn complete_part_upload(
        &self,
        key: &str,
        upload_id: &str,
        parts: Vec<CompletePartUploadParam>,
    ) -> Result<(), anyhow::Error> {
        let url = format!(
            "{}/buckets/{}/objects/{}/uploads/{upload_id}",
            self.region.get_upload_host(),
            self.bucket,
            self.get_base64encode_key(key)
        );
        let mut headers = self.get_part_headers(key);
        headers.insert(
            CONTENT_TYPE,
            APPLICATION_JSON.essence_str().try_into().unwrap(),
        );
        let mut data = HashMap::new();
        data.insert("parts", parts);
        let response = reqwest::Client::new()
            .post(url)
            .json(&data)
            .headers(headers)
            .send()
            .await?;
        if self.debug {
            println!("complete_part_upload response: {:#?}", response);
        }
        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to complete_part_upload: {} {}",
                response.status().as_u16(),
                response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string())
            ));
        }
        if self.debug {
            println!("complete_part_upload response: {:#?}", response);
        }
        Ok(())
    }

    /// # 分片上传 v2 版，带进度条
    /// <https://developer.qiniu.com/kodo/6364/multipartupload-interface>
    /// ## 参数
    /// - key 上传的key，如`test/Cargo.lock`
    /// - data R: AsyncReadExt + Unpin + Send + Sync + 'static
    /// - file_size: 文件大小，单位 bytes
    /// - part_size: 分片上传的大小，单位bytes，1M-1GB之间，如果指定，优先级比`threads`参数高
    /// - threads: 分片上传线程，在未指定`part_size`参数的情况下生效，默认5
    /// - progress_style: 进度条样式
    pub async fn part_upload_file_with_progress<R: AsyncReadExt + Unpin + Send + Sync + 'static>(
        self,
        key: &str,
        mut data: R,
        file_size: i64,
        part_size: Option<i64>,
        threads: Option<u8>,
        progress_style: Option<ProgressStyle>,
    ) -> Result<(), anyhow::Error> {
        let initiate = self.initial_part_upload(key).await?;
        let upload_id = initiate.upload_id;
        let mut part_number = 0;
        let mut upload_bytes = 0;
        let mut handles = Vec::new();
        let multi = MultiProgress::new();
        let sty = match progress_style {
            Some(sty)=>sty,
            None=> ProgressStyle::default_bar().template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})").unwrap().progress_chars("#>-")
        };
        // 单个 Part大小范围 1 MB - 1 GB，如果未指定part_size，默认5个线程
        let mut part_size = match part_size {
            Some(size) => size,
            None => file_size / threads.unwrap_or(5) as i64,
        };
        if part_size < 1024 * 1024 {
            part_size = 1024 * 1024;
        } else if part_size > 1024 * 1024 * 1024 {
            part_size = 1024 * 1024 * 1024;
        }
        loop {
            let last_bytes = file_size - upload_bytes;
            if last_bytes <= 0 {
                break;
            }
            let mut part_size1 = part_size as usize;
            // 倒数第二次上传后剩余小于1M，附加到倒数第二次上传
            if last_bytes - part_size < 1024 * 1024 && last_bytes < 1024 * 1024 * 1024 {
                part_size1 = last_bytes as usize;
            }
            let mut buf = vec![0; part_size1];
            data.read_exact(&mut buf).await?;
            part_number += 1;
            upload_bytes += buf.len() as i64;
            let this = self.clone();
            let key = key.to_string();
            let upload_id = upload_id.clone();
            let pb = multi.add(ProgressBar::new(buf.len() as u64));
            pb.set_style(sty.clone());
            let handle = tokio::spawn(async move {
                this.part_upload(&key, &upload_id, part_number, buf, pb)
                    .await
            });
            handles.push(handle);
        }
        let mut parts = Vec::new();
        for (i, handle) in handles.into_iter().enumerate() {
            let res = handle.await?.unwrap();
            parts.push(CompletePartUploadParam {
                etag: res.etag.clone(),
                part_number: (i + 1) as i64,
            });
        }
        if self.debug {
            println!("parts: {:#?}", parts);
        }
        // complete part upload
        self.complete_part_upload(key, &upload_id, parts).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use mime::APPLICATION_OCTET_STREAM;
    use tokio::fs;

    use super::*;

    #[tokio::test]
    async fn it_works() {
        let qiniu = QiniuUploader::new(
            String::from("access_key"),
            String::from("access_secret"),
            String::from("bucket"),
            Some(QiniuRegionEnum::Z0),
            true,
        );
        let mut f = fs::File::open("./Cargo.lock").await.unwrap();
        let file_size = f.metadata().await.unwrap().size();
        qiniu
            .part_upload_file_with_progress(
                "test/Cargo.lock",
                f,
                file_size as i64,
                Some(1024 * 1024 * 50),
                Some(10),
                None,
            )
            .await
            .unwrap();
    }
}
