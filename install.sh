#!/bin/sh
#
# aidev — install the latest release binary
#
# Downloads the newest aidev release, installs it to ~/bin, and makes it
# available on your PATH (persistently via ~/.<shell>rc when needed).
#
# Written in POSIX sh so it runs on minimal systems without bash.
#
set -eu

APP="aidev"
OWNER="aronym5"
REPO="aidev"
BIN_DIR="${AIDEV_BIN_DIR:-$HOME/bin}"
SHELLRC="${AIDEV_RC:-}"

# --- Pick a suitable shell rc file for persistent PATH -----------------
if [ -z "$SHELLRC" ]; then
  if [ -n "${ZSH_VERSION:-}" ]; then
    SHELLRC="$HOME/.zshrc"
  elif [ -n "${BASH_VERSION:-}" ]; then
    SHELLRC="$HOME/.bashrc"
  else
    SHELLRC="$HOME/.profile"
  fi
fi

# --- Resolve the latest release asset URL -----------------------------
# GitHub's /releases/latest redirects to the newest tagged release.
URL="https://github.com/${OWNER}/${REPO}/releases/latest/download/${APP}"

echo "==> Installing ${APP} to ${BIN_DIR}"

mkdir -p "${BIN_DIR}"

echo "==> Downloading latest release: ${URL}"
curl -fsSL "${URL}" -o "${BIN_DIR}/${APP}"

chmod +x "${BIN_DIR}/${APP}"

echo "==> Installed ${BIN_DIR}/${APP}"

# --- Ensure the binary is on PATH ------------------------------------
if command -v "${APP}" >/dev/null 2>&1; then
  echo "==> ${APP} is already on your PATH."
else
  case ":${PATH}:" in
    *":${BIN_DIR}:"*) ;;
    *)
      if [ -f "${SHELLRC}" ] || touch "${SHELLRC}" 2>/dev/null; then
        # Avoid inserting duplicate entries on re-runs.
        if ! grep -Fs "export PATH=\"${BIN_DIR}:\$PATH\"" "${SHELLRC}" >/dev/null 2>&1; then
          {
            echo ""
            echo "# added by aidev installer"
            echo "export PATH=\"${BIN_DIR}:\$PATH\""
          } >> "${SHELLRC}"
        fi
      fi
      export PATH="${BIN_DIR}:${PATH}"
      echo "==> Added ${BIN_DIR} to PATH (persisted in ${SHELLRC})."
      ;;
  esac
fi

echo
echo "Done! Run '${APP}' to start (restart your shell if '${APP}' is not found)."
