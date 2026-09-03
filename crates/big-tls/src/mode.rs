// Copyright 2026 Bany
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! What makes a file fit to hold a secret.
//!
//! Three files in this system are secrets on disk: the users file, the server's private key, and
//! the key a node presents to its peers. The rule for all three is the same one, so it is one
//! function here rather than a copy per caller that can drift - the file that grants access and
//! the key that carries it are not two policies.

use std::io;
use std::path::Path;

/// Refuses a file that anyone but its owner can open.
///
/// `what` names the file in the caller's words - "a users file", "a private key" - so the error
/// tells an operator which of the three they got wrong without them having to match a path.
///
/// Unix only, like the rest of the file-backed half of this tree: `MmapPager` is already
/// `#[cfg(unix)]`, so there is no platform where this check is skipped and a database is
/// nevertheless being served from a file.
#[cfg(unix)]
pub fn check_permissions(path: &Path, what: &str) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is mode {mode:04o}; {what} must not be readable by anyone else - \
                 run `chmod 600 {}`",
                path.display(),
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn check_permissions(_: &Path, _: &str) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file_at(mode: u32) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"x").unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        (dir, path)
    }

    #[test]
    #[cfg(unix)]
    fn a_world_readable_file_is_refused_and_says_how_to_fix_it() {
        let (_d, p) = file_at(0o644);
        let e = check_permissions(&p, "a users file").unwrap_err();
        assert!(e.to_string().contains("chmod 600"), "{e}");
        assert!(e.to_string().contains("a users file"), "the message names what it is: {e}");
    }

    #[test]
    #[cfg(unix)]
    fn a_group_readable_file_is_refused_too() {
        // 0o640 is the mode somebody reaches for when they want their deployment group to read
        // it, and a group is not an owner.
        let (_d, p) = file_at(0o640);
        assert!(check_permissions(&p, "a private key").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn mode_600_is_accepted() {
        let (_d, p) = file_at(0o600);
        check_permissions(&p, "a users file").unwrap();
    }

    #[test]
    fn a_missing_file_is_an_error_rather_than_a_pass() {
        let e =
            check_permissions(Path::new("/nonexistent/big/secret"), "a users file").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }
}
