use serde::Serialize;

/// 业务错误：携带 HTTP 语义（状态码 + 错误码），外部可依赖 code 而非 message。
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct BusinessError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

/// 全局统一错误。所有层只返回本类型；HTTP 状态码与错误码映射集中在 api/error.rs。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Business(#[from] Box<BusinessError>),

    #[error("数据库错误: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP 客户端错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Zip 错误: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("连接池错误: {0}")]
    Pool(String),
}

impl AppError {
    pub fn business(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        AppError::Business(Box::new(BusinessError {
            status,
            code,
            message: message.into(),
        }))
    }

    /// 400 INVALID_REQUEST
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::business(400, "INVALID_REQUEST", message)
    }

    /// 404（code 由调用方给出，如 ENVIRONMENT_NOT_FOUND）
    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::business(404, code, message)
    }

    /// 409（code 由调用方给出，如 INSTANCE_ALREADY_RUNNING）
    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::business(409, code, message)
    }

    /// 500 INTERNAL_ERROR
    pub fn internal(message: impl Into<String>) -> Self {
        Self::business(500, "INTERNAL_ERROR", message)
    }

    pub fn code(&self) -> &'static str {
        match self {
            AppError::Business(b) => b.code,
            _ => "INTERNAL_ERROR",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            AppError::Business(b) => b.status,
            _ => 500,
        }
    }
}

/// Tauri command 序列化形态。
impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("AppError", 2)?;
        s.serialize_field("code", self.code())?;
        s.serialize_field("message", &self.to_string())?;
        s.end()
    }
}

impl From<r2d2::Error> for AppError {
    fn from(e: r2d2::Error) -> Self {
        AppError::Pool(e.to_string())
    }
}
