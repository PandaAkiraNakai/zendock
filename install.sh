#!/usr/bin/env bash
# Instala ZenDock para el usuario actual, dentro de ~/.local. No toca el sistema
# ni pide root. Compila el binario de Rust (rust/) si hay cargo; si no, usa uno
# ya compilado en rust/target/release/zendock.
set -euo pipefail

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONF="${XDG_CONFIG_HOME:-$HOME/.config}"
SHARE="$DATA/zendock"
BIN="$HOME/.local/bin"
ORIGEN="$(cd "$(dirname "$0")" && pwd)"
BINARIO="$ORIGEN/rust/target/release/zendock"

autostart=0
compilar=1
for arg in "$@"; do
    case "$arg" in
        --autostart) autostart=1 ;;
        --sin-compilar) compilar=0 ;;
        -h|--help)
            echo "uso: $0 [--autostart] [--sin-compilar]"
            echo "  --autostart     arranca con la sesión, en segundo plano (solo bandeja)"
            echo "  --sin-compilar  usa el binario ya compilado en rust/target/release/"
            exit 0 ;;
        *) echo "opción desconocida: $arg" >&2; exit 1 ;;
    esac
done

if [ "$compilar" = 1 ] && command -v cargo >/dev/null; then
    echo "compilando (cargo build --release)…"
    cargo build --release --manifest-path "$ORIGEN/rust/Cargo.toml"
elif [ ! -x "$BINARIO" ]; then
    echo "hace falta cargo para compilar ZenDock (Arch: pacman -S rust; o https://rustup.rs)" >&2
    exit 1
fi

# Sin acceso al socket la app arranca igual, pero solo puede decir que no llega a Docker
case "${DOCKER_HOST:-}" in
    unix://*) SOCK="${DOCKER_HOST#unix://}" ;;
    *)        SOCK=/var/run/docker.sock ;;
esac
if [ -e "$SOCK" ] && [ ! -w "$SOCK" ]; then
    echo "AVISO: tu usuario no puede usar $SOCK. Arréglalo con:"
    echo "       sudo usermod -aG docker \$USER   # y volver a iniciar sesión"
fi

install -d "$SHARE" "$BIN" "$DATA/applications"
# install crea un fichero nuevo: un zendock que ya esté corriendo sigue con el viejo
install -m 755 "$BINARIO" "$BIN/zendock"
# restos de la versión en Python
rm -rf "$SHARE/zendock.py" "$SHARE/__pycache__"

# El icono lo dibuja el propio binario: así no hace falta guardar ningún PNG en el repo
"$BIN/zendock" --write-icon "$SHARE/zendock.png"

cat > "$DATA/applications/zendock.desktop" <<ESCRITORIO
[Desktop Entry]
Type=Application
Name=ZenDock
GenericName=Contenedores Docker
Comment=Estado de los contenedores Docker: encender, apagar, reiniciar y abrir sus webs
Exec=$BIN/zendock
Icon=$SHARE/zendock.png
Terminal=false
Categories=System;Monitor;
Keywords=docker;contenedores;containers;compose;zendock;
StartupNotify=true
StartupWMClass=zendock
ESCRITORIO

# KWin no deja que una ventana ya abierta se traiga al frente sola: al pedir
# activación sin un token de Wayland la marca como que "reclama atención" (el
# parpadeo naranja en la barra de tareas) en vez de levantarla. Una regla de
# ventana que desactive esa protección solo para ZenDock lo arregla. (En
# Hyprland no hace falta: la segunda invocación usa hyprctl.)
REGLA_ID="003852a2-9a52-45ba-ad0f-6236108cb53e"
anadir_regla_kwin() {
    command -v kwriteconfig6 >/dev/null || return 0
    local previas
    previas="$(kreadconfig6 --file kwinrulesrc --group General --key rules 2>/dev/null || true)"
    case ",$previas," in
        *",$REGLA_ID,"*) return 0 ;;        # ya estaba, no duplicar
    esac
    kwriteconfig6 --file kwinrulesrc --group "$REGLA_ID" --key Description \
        "ZenDock: dejar que se traiga al frente sola"
    kwriteconfig6 --file kwinrulesrc --group "$REGLA_ID" --key wmclass zendock
    kwriteconfig6 --file kwinrulesrc --group "$REGLA_ID" --key wmclassmatch 1
    kwriteconfig6 --file kwinrulesrc --group "$REGLA_ID" --key fsplevel 0
    kwriteconfig6 --file kwinrulesrc --group "$REGLA_ID" --key fsplevelrule 2
    kwriteconfig6 --file kwinrulesrc --group General --key rules \
        "${previas:+$previas,}$REGLA_ID"
    kwriteconfig6 --file kwinrulesrc --group General --key count \
        "$(( $(printf '%s' "${previas:+$previas,}$REGLA_ID" | tr ',' '\n' | grep -c .) ))"
    gdbus call --session --dest org.kde.KWin --object-path /KWin \
        --method org.kde.KWin.reconfigure >/dev/null 2>&1 || true
    echo "regla de KWin añadida (la ventana ya se trae al frente sola)"
}
anadir_regla_kwin

if [ "$autostart" = 1 ]; then
    install -d "$CONF/autostart"
    cat > "$CONF/autostart/zendock.desktop" <<AUTO
[Desktop Entry]
Type=Application
Name=ZenDock
Comment=Arranca en segundo plano, solo con el icono en la bandeja
Exec=$BIN/zendock --tray
Icon=$SHARE/zendock.png
Terminal=false
StartupNotify=false
X-GNOME-Autostart-enabled=true
AUTO
    echo "autoarranque activado (modo bandeja)"
fi

command -v update-desktop-database >/dev/null && \
    update-desktop-database "$DATA/applications" 2>/dev/null || true

echo "instalado. Arráncalo con: $BIN/zendock --tray   (o abre la ventana con: $BIN/zendock)"
if pgrep -x zendock >/dev/null 2>&1 || pgrep -f "zendock/[z]endock\.py" >/dev/null 2>&1; then
    echo "ojo: hay un ZenDock en marcha con la versión anterior; ciérralo (menú de la bandeja > Salir)"
    echo "     y vuelve a lanzar $BIN/zendock --tray para usar la nueva"
fi
case ":$PATH:" in
    *":$BIN:"*) ;;
    *) echo "ojo: $BIN no está en tu PATH" ;;
esac
