pub(super) fn cmd_shim(relative_path: &str, use_deno: bool) -> String {
  const FIND_NODE: &str = r#"IF EXIST "%dp0%\node.exe" (
  SET "_prog=%dp0%\node.exe"
) ELSE (
  SET "_prog=node"
  SET PATHEXT=%PATHEXT:;.JS;=;%
)"#;
  format!(
    r#"@ECHO off
GOTO start
:find_dp0
SET dp0=%~dp0
EXIT /b
:start
SETLOCAL
CALL :find_dp0

{find_node}


endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & {call} "%dp0%\{relative_path}" %*
"#,
    find_node = if use_deno { FIND_NODE } else { "" },
    call = if use_deno {
      "deno run -A"
    } else {
      "\"%_prog%\"" // node
    }
  )
}
