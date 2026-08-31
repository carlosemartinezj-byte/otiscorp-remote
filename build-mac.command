#!/bin/bash
# ============================================================================
# OtisCorp Remote — Generador del instalador de macOS (.dmg)
#
# Haz DOBLE CLIC en este archivo en un Mac. Instala solo lo que falte
# (herramientas de Xcode, Rust y Tauri) y genera el instalador .dmg.
# Solo hay que hacerlo UNA VEZ, en UN Mac. Las demas Mac solo abren el .dmg.
# ============================================================================
set -e
cd "$(dirname "$0")"

echo ""
echo "==================================================="
echo "  OtisCorp Remote — construyendo instalador de Mac"
echo "==================================================="
echo ""

# --- 1) Herramientas de linea de comandos de Xcode -------------------------
if ! xcode-select -p >/dev/null 2>&1; then
  echo ">> Faltan las 'Command Line Tools' de Xcode. Abriendo el instalador…"
  echo "   Acepta la ventana de Apple que aparece, espera a que termine,"
  echo "   y vuelve a hacer doble clic en este archivo."
  xcode-select --install || true
  read -n 1 -s -r -p "Pulsa una tecla para cerrar…"
  exit 0
fi
echo ">> Xcode Command Line Tools: OK"

# --- 2) Rust (cargo) --------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  echo ">> Instalando Rust (automatico)…"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
# Cargar Rust en esta sesion aunque se acabe de instalar.
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
echo ">> Rust: OK ($(cargo --version))"

# --- 3) Tauri CLI (no necesita Node) ---------------------------------------
if ! cargo tauri --version >/dev/null 2>&1; then
  echo ">> Instalando Tauri CLI…"
  cargo install tauri-cli --version "^2" --locked
fi
echo ">> Tauri CLI: OK"

# --- 4) Compilar el instalador ---------------------------------------------
echo ""
echo ">> Compilando (la PRIMERA vez tarda varios minutos)…"
cargo tauri build

echo ""
echo "==================================================="
echo "  LISTO. Tu instalador de Mac (.dmg) esta en:"
echo "  src-tauri/target/release/bundle/dmg/"
echo "==================================================="
open "src-tauri/target/release/bundle/dmg/" 2>/dev/null || true
read -n 1 -s -r -p "Pulsa una tecla para cerrar…"
