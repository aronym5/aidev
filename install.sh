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

# Install atomically: in-place writes to a *running* binary fail with
# ETXTBSY ("Text file busy"), so download to a temp file in the same
# directory and rename it over the target. This works even while aidev
# is currently running; the old version stays in memory for the running
# instance until it is restarted.
TMP="${BIN_DIR}/.${APP}.$$"
if ! curl -fsSL "${URL}" -o "${TMP}"; then
  rm -f "${TMP}"
  echo "==> ERROR: download failed; existing ${BIN_DIR}/${APP} was left untouched." >&2
  exit 1
fi

chmod +x "${TMP}"

if [ -d "${BIN_DIR}/${APP}" ]; then
  rm -f "${TMP}"
  echo "==> ERROR: ${BIN_DIR}/${APP} is a directory, not a file." >&2
  exit 1
fi

mv -f "${TMP}" "${BIN_DIR}/${APP}"

if [ ! -x "${BIN_DIR}/${APP}" ]; then
  echo "==> ERROR: downloaded file is not a runnable binary." >&2
  exit 1
fi

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
echo "Note: an already running ${APP} keeps using the previous version until you restart it."
