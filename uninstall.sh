#!/usr/bin/env bash
# Deja el sistema como estaba: borra todo lo que pone install.sh. No toca
# enlaces.json (~/.config/zendock), que es configuracion tuya.
set -euo pipefail

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
CONF="${XDG_CONFIG_HOME:-$HOME/.config}"
BIN="$HOME/.local/bin"

pkill -f "zendock/[z]endock\.py" 2>/dev/null || true

rm -rf "$DATA/zendock"
rm -f  "$BIN/zendock"
rm -f  "$DATA/applications/zendock.desktop"
rm -f  "$CONF/autostart/zendock.desktop"
rm -f  "/tmp/zendock-$(id -u)"

# quita la regla de KWin, dejando intactas las demas
REGLA_ID="003852a2-9a52-45ba-ad0f-6236108cb53e"
if command -v kwriteconfig6 >/dev/null; then
    previas="$(kreadconfig6 --file kwinrulesrc --group General --key rules 2>/dev/null || true)"
    case ",$previas," in
        *",$REGLA_ID,"*)
            quedan="$(printf '%s' "$previas" | tr ',' '\n' | grep -vx "$REGLA_ID" \
                      | paste -sd, -)"
            kwriteconfig6 --file kwinrulesrc --group General --key rules "$quedan"
            kwriteconfig6 --file kwinrulesrc --group General --key count \
                "$(printf '%s' "$quedan" | tr ',' '\n' | grep -c . || true)"
            # kwriteconfig6 no borra grupos: se vacia clave a clave y KConfig
            # se lleva por delante el grupo vacio al guardar
            for clave in Description wmclass wmclassmatch fsplevel fsplevelrule; do
                kwriteconfig6 --file kwinrulesrc --group "$REGLA_ID" --key "$clave" --delete
            done
            gdbus call --session --dest org.kde.KWin --object-path /KWin \
                --method org.kde.KWin.reconfigure >/dev/null 2>&1 || true
            ;;
    esac
fi
echo "desinstalado"
