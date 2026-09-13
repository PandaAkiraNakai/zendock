#!/usr/bin/env bash
# Instala ZenDock para el usuario actual, dentro de ~/.local. No toca el sistema
# ni pide root. Las rutas se generan aqui, por eso el repo no lleva ninguna fija.
set -euo pipefail

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONF="${XDG_CONFIG_HOME:-$HOME/.config}"
SHARE="$DATA/zendock"
BIN="$HOME/.local/bin"
ORIGEN="$(cd "$(dirname "$0")" && pwd)"

autostart=0
for arg in "$@"; do
    case "$arg" in
        --autostart) autostart=1 ;;
        -h|--help)
            echo "uso: $0 [--autostart]"
            echo "  --autostart  arranca con la sesion, en segundo plano (solo bandeja)"
            exit 0 ;;
        *) echo "opcion desconocida: $arg" >&2; exit 1 ;;
    esac
done

command -v python3 >/dev/null || { echo "hace falta python3" >&2; exit 1; }
python3 -c "import PyQt6.QtWidgets, PyQt6.QtNetwork" 2>/dev/null || {
    echo "hace falta PyQt6 (Arch: pacman -S python-pyqt6)" >&2; exit 1; }

# Sin acceso al socket la app arranca igual, pero solo puede decir que no llega a Docker
case "${DOCKER_HOST:-}" in
    unix://*) SOCK="${DOCKER_HOST#unix://}" ;;
    *)        SOCK=/var/run/docker.sock ;;
esac
if [ ! -w "$SOCK" ]; then
    echo "AVISO: tu usuario no puede usar $SOCK. Arreglalo con:"
    echo "       sudo usermod -aG docker \$USER   # y volver a iniciar sesion"
fi

install -d "$SHARE" "$BIN" "$DATA/applications"
install -m 644 "$ORIGEN/zendock.py" "$SHARE/zendock.py"

# El icono se dibuja desde el vector que ya vive en el codigo, asi no hace falta
# guardar ningun binario en el repositorio.
QT_QPA_PLATFORM=offscreen python3 - "$SHARE" <<'PY'
import sys
from PyQt6.QtWidgets import QApplication
sys.path.insert(0, sys.argv[1])
app = QApplication([])
import zendock as z
z.dock_icon(z.BLUE, 256).pixmap(256, 256).save(sys.argv[1] + "/zendock.png")
PY
rm -rf "$SHARE/__pycache__"

cat > "$BIN/zendock" <<LANZADOR
#!/bin/sh
exec python3 "$SHARE/zendock.py" "\$@"
LANZADOR
chmod +x "$BIN/zendock"

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
ESCRITORIO

# KWin no deja que una ventana ya abierta se traiga al frente sola: al pedir
# activacion sin un token de Wayland la marca como que "reclama atencion" (el
# parpadeo naranja en la barra de tareas) en vez de levantarla. Una regla de
# ventana que desactive esa proteccion solo para ZenDock lo arregla.
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
    echo "regla de KWin anadida (la ventana ya se trae al frente sola)"
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

echo "instalado. Arrancalo con: $BIN/zendock"
case ":$PATH:" in
    *":$BIN:"*) ;;
    *) echo "ojo: $BIN no esta en tu PATH" ;;
esac
