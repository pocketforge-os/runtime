//! The W0 fetch reason codes pf-app-fetch emits (mission-control
//! `.planning/infra/infra-107.4-fetch-reason-codes.toml`, scope "all", as amended by W3 with
//! the `source_moved` class for `redirect_refused`).

use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Offline,
    HttpError,
    HttpRefused,
    RedirectRefused,
    DownloadTooLarge,
    DownloadTruncated,
    SizeMismatch,
    DigestMismatch,
    ArchiveInvalid,
    ArchiveMemberUnsafe,
    DiskFull,
    Cancelled,
    /// Reported by pf-app-fetch as the step-2 `check` verdict (a stale receipt is
    /// replaced, not a failure); the launcher (W4) emits it as a reason.
    #[allow(dead_code)]
    ReceiptStale,
    StateInvalid,
    DescriptorInvalid,
}

impl Code {
    #[cfg_attr(not(test), allow(dead_code))]
    pub const ALL: [Code; 15] = [
        Code::Offline,
        Code::HttpError,
        Code::HttpRefused,
        Code::RedirectRefused,
        Code::DownloadTooLarge,
        Code::DownloadTruncated,
        Code::SizeMismatch,
        Code::DigestMismatch,
        Code::ArchiveInvalid,
        Code::ArchiveMemberUnsafe,
        Code::DiskFull,
        Code::Cancelled,
        Code::ReceiptStale,
        Code::StateInvalid,
        Code::DescriptorInvalid,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Code::Offline => "offline",
            Code::HttpError => "http_error",
            Code::HttpRefused => "http_refused",
            Code::RedirectRefused => "redirect_refused",
            Code::DownloadTooLarge => "download_too_large",
            Code::DownloadTruncated => "download_truncated",
            Code::SizeMismatch => "size_mismatch",
            Code::DigestMismatch => "digest_mismatch",
            Code::ArchiveInvalid => "archive_invalid",
            Code::ArchiveMemberUnsafe => "archive_member_unsafe",
            Code::DiskFull => "disk_full",
            Code::Cancelled => "cancelled",
            Code::ReceiptStale => "receipt_stale",
            Code::StateInvalid => "state_invalid",
            Code::DescriptorInvalid => "descriptor_invalid",
        }
    }

    /// The `[classes]` key the launcher (W4) shows text for.
    pub fn class(self) -> &'static str {
        match self {
            Code::Offline | Code::DownloadTruncated => "network",
            Code::HttpError => "server",
            Code::HttpRefused => "source_gone",
            Code::RedirectRefused => "source_moved",
            Code::DownloadTooLarge | Code::SizeMismatch | Code::DigestMismatch => "source_mismatch",
            Code::ArchiveInvalid | Code::ArchiveMemberUnsafe | Code::DescriptorInvalid => {
                "app_defect"
            }
            Code::DiskFull => "storage",
            Code::Cancelled => "cancelled",
            Code::ReceiptStale | Code::StateInvalid => "setup_required",
        }
    }

    pub fn retryable(self) -> bool {
        matches!(
            self,
            Code::Offline
                | Code::HttpError
                | Code::DownloadTruncated
                | Code::DiskFull
                | Code::Cancelled
                | Code::ReceiptStale
                | Code::StateInvalid
        )
    }
}

/// A failed run: the code plus a short operator detail (never shown as user text).
#[derive(Debug)]
pub struct Failure {
    pub code: Code,
    pub detail: String,
}

impl Failure {
    pub fn new(code: Code, detail: impl Into<String>) -> Failure {
        Failure {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

pub type Result<T> = std::result::Result<T, Failure>;

/// ENOSPC / EDQUOT is `disk_full`; anything else is the caller's `code`.
pub fn io_failure(code: Code, what: &str, error: &std::io::Error) -> Failure {
    match error.raw_os_error() {
        Some(libc::ENOSPC) | Some(libc::EDQUOT) => {
            Failure::new(Code::DiskFull, format!("{what}: {error}"))
        }
        _ => Failure::new(code, format!("{what}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The W0 table (TOML `[codes.*]`, scope "all") with W3's `source_moved` remap.
    #[test]
    fn table_matches_the_w0_contract() {
        let want: &[(&str, &str, bool)] = &[
            ("offline", "network", true),
            ("http_error", "server", true),
            ("http_refused", "source_gone", false),
            ("redirect_refused", "source_moved", false),
            ("download_too_large", "source_mismatch", false),
            ("download_truncated", "network", true),
            ("size_mismatch", "source_mismatch", false),
            ("digest_mismatch", "source_mismatch", false),
            ("archive_invalid", "app_defect", false),
            ("archive_member_unsafe", "app_defect", false),
            ("disk_full", "storage", true),
            ("cancelled", "cancelled", true),
            ("receipt_stale", "setup_required", true),
            ("state_invalid", "setup_required", true),
            ("descriptor_invalid", "app_defect", false),
        ];
        let got: Vec<(&str, &str, bool)> = Code::ALL
            .iter()
            .map(|c| (c.as_str(), c.class(), c.retryable()))
            .collect();
        assert_eq!(got, want);
    }
}
