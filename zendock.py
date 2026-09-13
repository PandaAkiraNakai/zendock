#!/usr/bin/env python3
"""ZenDock - los contenedores Docker del portatil, con la pinta de ZenMonitor.

Muestra cada contenedor agrupado por su proyecto de compose, con estado, salud,
tiempo en marcha y consumo, y deja encenderlos, apagarlos y reiniciarlos (uno a
uno o el proyecto entero), abrir la web que sirven, lanzar la app de escritorio
que los acompana, seguir sus logs o abrir una shell dentro.

Habla directamente con el socket de Docker (la API HTTP del Engine) usando la
biblioteca estandar, sin lanzar un `docker` por cada lectura: listar e
inspeccionar los once contenedores del portatil tarda unos 30 ms y las
estadisticas de cada uno unos 6 ms. Los cambios de estado llegan al instante por
el flujo /events; el sondeo cada 2 s solo hace falta para la CPU y la RAM.

Las webs se descubren solas: a cada puerto TCP publicado se le hace un GET y
cuenta como web si responde HTML o una redireccion (una API que contesta JSON no
cuenta). Un contenedor con red del host no publica puertos, asi que para esos se
prueban los puertos que declara la imagen, salvo los que ya publica otro
contenedor: cronos-backend declara el 8080 pero ese es el de Zabbix. Lo que no se
pueda adivinar se fija con la etiqueta `zendock.web` o en enlaces.json.
"""

import configparser
import http.client
import json
import os
import re
import shlex
import shutil
import socket
import ssl
import subprocess
import sys
import threading
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

from PyQt6.QtCore import (QObject, QProcess, QRectF, QSettings, QSize, Qt,
                          QThread, QUrl, pyqtSignal)
from PyQt6.QtGui import (QAction, QColor, QDesktopServices, QIcon, QPainter,
                         QPainterPath, QPixmap)
from PyQt6.QtNetwork import QLocalServer, QLocalSocket
from PyQt6.QtWidgets import (QApplication, QFrame, QHBoxLayout, QLabel, QMenu,
                             QPushButton, QScrollArea, QSizePolicy,
                             QSystemTrayIcon, QToolButton, QVBoxLayout, QWidget)

POLL_S = 2.0
PROBE_TIMEOUT_S = 0.8
APPS_RESCAN_S = 30
INTENCIONADO_S = 15     # un kill/stop reciente convierte el "die" en un apagado normal
IPC = f"zendock-{os.getuid()}"

HOME = str(Path.home())
CONFIG = Path(os.environ.get("XDG_CONFIG_HOME") or f"{HOME}/.config") / "zendock" / "enlaces.json"
APP_DIRS = (Path(os.environ.get("XDG_DATA_HOME") or f"{HOME}/.local/share") / "applications",
            Path("/usr/local/share/applications"), Path("/usr/share/applications"))

# Puertos de protocolos que no son HTTP: no se les manda un GET de prueba
NO_HTTP = {21, 22, 23, 25, 49, 53, 110, 143, 389, 445, 636, 1433, 1521, 1883, 2049,
           3306, 5432, 5672, 6379, 9042, 10050, 10051, 11211, 27017}
# Salidas normales: 0, o las de `docker stop` (SIGTERM y, si no hizo caso, SIGKILL)
SALIDA_NORMAL = {0, 137, 143}

BLUE = QColor("#3daee9")
AMBER = QColor("#e8a33d")
RED = QColor("#da4453")
GREEN = QColor("#3ec46d")
GREY = QColor("#7f8c8d")


# ------------------------------------------------------------------- Docker API
class DockerError(Exception):
    pass


class UnixHTTPConnection(http.client.HTTPConnection):
    def __init__(self, path, timeout):
        super().__init__("docker", timeout=timeout)
        self.socket_path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.socket_path)


def socket_error(exc, path):
    if isinstance(exc, PermissionError):
        return f"sin permiso para {path} (¿tu usuario está en el grupo docker?)"
    if isinstance(exc, (FileNotFoundError, ConnectionRefusedError)):
        return "Docker no está en marcha"
    return f"sin conexión con Docker ({exc})"


class Docker:
    """Cliente minimo de la API del Engine sobre el socket unix."""

    def __init__(self):
        host = os.environ.get("DOCKER_HOST", "")
        self.path = host[len("unix://"):] if host.startswith("unix://") else "/var/run/docker.sock"

    def call(self, method, url, timeout=10):
        conn = UnixHTTPConnection(self.path, timeout)
        try:
            conn.request(method, url, headers={"Content-Length": "0"} if method == "POST" else {})
            resp = conn.getresponse()
            data = resp.read()
        except (OSError, http.client.HTTPException) as exc:
            raise DockerError(socket_error(exc, self.path)) from exc
        finally:
            conn.close()
        if resp.status >= 400:
            try:
                msg = json.loads(data).get("message")
            except ValueError:
                msg = data.decode(errors="replace").strip()
            raise DockerError(msg or f"HTTP {resp.status}")
        return json.loads(data) if data else None

    def stream(self, url):
        conn = UnixHTTPConnection(self.path, None)
        try:
            conn.request("GET", url)
            resp = conn.getresponse()
        except OSError as exc:
            conn.close()
            raise DockerError(socket_error(exc, self.path)) from exc
        if resp.status >= 400:
            conn.close()
            raise DockerError(f"HTTP {resp.status}")
        return conn, resp


def docker_time(value):
    """Marca de tiempo de Docker (RFC 3339 con nanosegundos) a epoch; 0 si nunca."""
    if not value or value.startswith("0001-"):
        return 0.0
    value = re.sub(r"(\.\d{6})\d+", r"\1", value).replace("Z", "+00:00")
    try:
        return datetime.fromisoformat(value).timestamp()
    except ValueError:
        return 0.0


# ---------------------------------------------------------------------- modelo
@dataclass(frozen=True)
class DesktopApp:
    id: str
    name: str
    icon: str
    path: str
    target: str     # ejecutable ya resuelto (sigue enlaces simbolicos)
    text: str       # contenido del ejecutable si es un script de $HOME


@dataclass
class Ctr:
    id: str
    name: str
    image: str
    project: str
    service: str
    workdir: str
    config_files: list
    state: str          # running, exited, paused, restarting, created, dead
    health: str         # healthy, unhealthy, starting o ""
    exit_code: int
    started: float
    finished: float
    restarts: int
    oom: bool
    restart_policy: str
    ports: list         # [(ip del host, puerto del host, puerto del contenedor)] solo TCP
    exposed: list       # puertos TCP que declara la imagen
    net_mode: str
    ips: dict           # red -> IP
    labels: dict
    cpu: float | None = None
    mem: int | None = None
    webs: list = field(default_factory=list)    # [(nombre, url)]
    apps: list = field(default_factory=list)    # [DesktopApp], solo contenedores sueltos

    @property
    def alive(self):
        return self.state in ("running", "paused", "restarting")


@dataclass
class Snapshot:
    containers: list
    project_apps: dict
    ncpu: int
    mem_total: int


@dataclass
class Group:
    key: str
    title: str
    project: str
    workdir: str
    config_files: list
    ctrs: list


def group_containers(ctrs):
    groups = {}
    for c in ctrs:
        g = groups.setdefault(c.project, Group(c.project, c.project or "Sueltos", c.project,
                                               c.workdir, c.config_files, []))
        g.ctrs.append(c)
    ordered = sorted((g for k, g in groups.items() if k), key=lambda g: g.project)
    if "" in groups:
        ordered.append(groups[""])
    for g in ordered:
        g.ctrs.sort(key=lambda c: c.name)
    return ordered


def parse_links(value):
    """'Nombre=url, url2' (o una lista) -> [(nombre, url)]."""
    items = value if isinstance(value, list) else str(value or "").split(",")
    links = []
    for item in items:
        item = str(item).strip()
        if not item:
            continue
        name, sep, url = item.partition("=")
        if not sep or "://" in name:
            name, url = "", item
        url = url.strip()
        links.append((name.strip() or urllib.parse.urlsplit(url).netloc or url, url))
    return links


_config = {"mtime": None, "data": {}, "error": ""}


def load_config():
    """enlaces.json: {"contenedor o proyecto": {"web": [...], "app": "id.desktop"}}."""
    try:
        mtime = CONFIG.stat().st_mtime
    except OSError:
        _config.update(mtime=None, data={}, error="")
        return _config
    if mtime != _config["mtime"]:
        try:
            data = json.loads(CONFIG.read_text())
            if not isinstance(data, dict):
                raise ValueError("tiene que ser un objeto")
            _config.update(mtime=mtime, data=data, error="")
        except (OSError, ValueError) as exc:
            _config.update(mtime=mtime, data={}, error=f"{CONFIG}: {exc}")
    return _config


def scan_apps():
    apps, seen = [], set()
    for folder in APP_DIRS:
        for path in sorted(folder.glob("*.desktop")):
            if path.name in seen:
                continue
            seen.add(path.name)
            parser = configparser.RawConfigParser(strict=False, interpolation=None)
            parser.optionxform = str
            try:
                parser.read(path, encoding="utf-8")
            except (configparser.Error, UnicodeDecodeError, OSError):
                continue
            if not parser.has_section("Desktop Entry"):
                continue
            entry = parser["Desktop Entry"]
            if entry.get("Type", "Application") != "Application" or \
                    entry.get("Hidden", "").lower() == "true":
                continue
            try:
                exe = shlex.split(entry.get("Exec", ""))[0]
            except (ValueError, IndexError):
                exe = ""
            target = os.path.realpath(shutil.which(exe) or exe) if exe else ""
            text = ""
            if target.startswith(HOME + os.sep):
                try:
                    with open(target, "rb") as fh:
                        head = fh.read(200_000)
                    if b"\0" not in head[:1024]:
                        text = head.decode("utf-8", "replace")
                except OSError:
                    pass
            apps.append(DesktopApp(path.stem, entry.get("Name[es]") or entry.get("Name") or path.stem,
                                   entry.get("Icon", ""), str(path), target, text))
    return apps


def match_apps(apps, key, workdir, config, label):
    """Apps de escritorio de un proyecto (o de un contenedor suelto)."""
    wanted = [v.strip() for v in (label or "").split(",") if v.strip()]
    configured = (config.get(key) or {}).get("app") or []
    wanted += [configured] if isinstance(configured, str) else list(configured)
    if wanted:
        ids = [w.removesuffix(".desktop") for w in wanted]
        return [a for w in ids for a in apps if a.id == w]
    found = []
    for app in apps:
        if app.id == "zendock":
            continue
        by_name = app.id == key
        by_dir = bool(workdir) and (
            app.target.startswith(workdir + os.sep)
            or re.search(re.escape(workdir) + r"(?![\w.-])", app.text) is not None)
        if by_name or by_dir:
            found.append(app)
    return found


def probe_web(host, port):
    """(url, definitivo). url si el puerto sirve una pagina web (HTML o redireccion).

    definitivo=False cuando nadie contesto: el servicio puede estar arrancando
    (Apache tarda un segundo en abrir el puerto) y hay que volver a probar."""
    url_host = f"[{host}]" if ":" in host else host
    schemes = ("https", "http") if str(port).endswith("443") else ("http", "https")
    answered = False
    for scheme in schemes:
        try:
            if scheme == "https":
                conn = http.client.HTTPSConnection(host, port, timeout=PROBE_TIMEOUT_S,
                                                   context=ssl._create_unverified_context())
            else:
                conn = http.client.HTTPConnection(host, port, timeout=PROBE_TIMEOUT_S)
            conn.request("GET", "/", headers={"User-Agent": "ZenDock", "Accept": "text/html"})
            resp = conn.getresponse()
            ctype = resp.getheader("Content-Type") or ""
            conn.close()
        except http.client.RemoteDisconnected:
            continue        # acepto y colgo: suele ser un servicio a medio arrancar
        except (http.client.HTTPException, ssl.SSLError):
            answered = True  # contesta, pero en otro protocolo (p. ej. TLS en el http)
            continue
        except OSError:
            continue        # rechazada o sin respuesta
        if 300 <= resp.status < 400 or ("html" in ctype and resp.status < 500):
            return f"{scheme}://{url_host}:{port}/", True
        return None, True   # habla HTTP pero no es una pagina: una API
    return None, answered


def ago(seconds):
    s = max(0, int(seconds))
    if s < 60:
        return f"{s} s"
    if s < 3600:
        return f"{s // 60} min"
    if s < 86400:
        return f"{s // 3600} h"
    return "1 día" if s < 172800 else f"{s // 86400} días"


def size(n):
    if n is None:
        return "--"
    if n >= 1024 ** 3:
        return f"{n / 1024 ** 3:.1f} GiB"
    return f"{n / 1024 ** 2:.0f} MiB"


def status_of(c):
    """Color del punto y texto de estado, en castellano."""
    now = time.time()
    if c.state == "running":
        text = f"en marcha · {ago(now - c.started)}"
        if c.health == "unhealthy":
            return AMBER, text + " · no sano"
        if c.health == "starting":
            return AMBER, text + " · arrancando"
        return GREEN, text + (" · sano" if c.health == "healthy" else "")
    if c.state == "paused":
        return AMBER, "en pausa"
    if c.state == "restarting":
        return RED, f"reiniciándose en bucle ({c.restarts})"
    if c.state == "exited":
        text = f"parado hace {ago(now - c.finished)}" if c.finished else "parado"
        if c.oom:
            return RED, text + " · se quedó sin memoria"
        if c.exit_code not in SALIDA_NORMAL:
            return RED, text + f" · código {c.exit_code}"
        return GREY, text
    if c.state == "created":
        return GREY, "creado, sin arrancar"
    if c.state == "dead":
        return RED, "muerto"
    return GREY, c.state


def is_trouble(c):
    return c.state in ("restarting", "dead") or (c.state == "running" and c.health == "unhealthy")


# ------------------------------------------------------------------- lectura
class Poller(QThread):
    """Lee el estado completo cada POLL_S, o antes si llega un evento."""

    snapshot = pyqtSignal(object)
    failed = pyqtSignal(str)

    def __init__(self, docker):
        super().__init__()
        self.docker = docker
        self.wake = threading.Event()
        self.stopping = False
        self.prev_cpu = {}
        self.probes = {}        # (id, arranque, puerto) -> (url o None, definitivo, cuando)
        self.pending = []
        self.apps = []
        self.apps_at = -APPS_RESCAN_S
        self.ncpu = os.cpu_count() or 1
        self.mem_total = self._mem_total()

    @staticmethod
    def _mem_total():
        try:
            with open("/proc/meminfo") as fh:
                for line in fh:
                    if line.startswith("MemTotal:"):
                        return int(line.split()[1]) * 1024
        except OSError:
            pass
        return 0

    def run(self):
        while not self.stopping:
            try:
                self.snapshot.emit(self.collect())
                if self.pending:        # webs nuevas por descubrir: se prueban y se repinta
                    for key, host, port in self.pending:
                        self.probes[key] = (*probe_web(host, port), time.monotonic())
                    self.pending = []
                    self.snapshot.emit(self.collect())
            except DockerError as exc:
                self.failed.emit(str(exc))
            self.wake.wait(POLL_S)
            self.wake.clear()

    def stop(self):
        self.stopping = True
        self.wake.set()

    def collect(self):
        rows = self.docker.call("GET", "/containers/json?all=1")
        config = load_config()["data"]
        if time.monotonic() - self.apps_at > APPS_RESCAN_S:
            self.apps = scan_apps()
            self.apps_at = time.monotonic()

        ctrs = []
        for row in rows:
            try:
                ctrs.append(self.parse(self.docker.call("GET", f"/containers/{row['Id']}/json")))
            except DockerError:
                continue        # se borro entre la lista y la inspeccion
        published = {port for c in ctrs if c.alive for _ip, port, _cport in c.ports}

        for c in ctrs:
            if c.state == "running":
                self.stats(c)
            else:
                self.prev_cpu.pop(c.id, None)
            c.webs = self.webs_for(c, config, published)
            if not c.project:
                c.apps = match_apps(self.apps, c.name, "", config, c.labels.get("zendock.app"))

        project_apps = {}
        for g in group_containers(ctrs):
            if g.project:
                label = ",".join(c.labels["zendock.app"] for c in g.ctrs if "zendock.app" in c.labels)
                project_apps[g.project] = match_apps(self.apps, g.project, g.workdir, config, label)
        return Snapshot(ctrs, project_apps, self.ncpu, self.mem_total)

    @staticmethod
    def parse(info):
        labels = info["Config"].get("Labels") or {}
        state = info["State"]
        net = info["NetworkSettings"]
        ports, seen = [], set()
        for key, binds in (net.get("Ports") or {}).items():
            cport, _, proto = key.partition("/")
            for bind in binds or []:
                hport = int(bind["HostPort"])
                if proto == "tcp" and hport not in seen:     # 0.0.0.0 y :: son el mismo puerto
                    seen.add(hport)
                    ports.append((bind.get("HostIp", ""), hport, int(cport)))
        exposed = [int(k.partition("/")[0]) for k in (info["Config"].get("ExposedPorts") or {})
                   if k.endswith("/tcp")]
        files = [f for f in labels.get("com.docker.compose.project.config_files", "").split(",") if f]
        return Ctr(
            id=info["Id"], name=info["Name"].lstrip("/"), image=info["Config"].get("Image", ""),
            project=labels.get("com.docker.compose.project", ""),
            service=labels.get("com.docker.compose.service", ""),
            workdir=labels.get("com.docker.compose.project.working_dir", ""),
            config_files=files, state=state.get("Status", ""),
            health=(state.get("Health") or {}).get("Status", ""),
            exit_code=state.get("ExitCode", 0), started=docker_time(state.get("StartedAt")),
            finished=docker_time(state.get("FinishedAt")), restarts=info.get("RestartCount", 0),
            oom=state.get("OOMKilled", False),
            restart_policy=((info.get("HostConfig") or {}).get("RestartPolicy") or {}).get("Name", ""),
            ports=ports, exposed=exposed,
            net_mode=(info.get("HostConfig") or {}).get("NetworkMode", ""),
            ips={k: v.get("IPAddress") for k, v in (net.get("Networks") or {}).items()
                 if v.get("IPAddress")},
            labels=labels)

    def stats(self, c):
        try:
            s = self.docker.call("GET", f"/containers/{c.id}/stats?stream=false&one-shot=true")
        except DockerError:
            return
        cpu = s.get("cpu_stats") or {}
        total = (cpu.get("cpu_usage") or {}).get("total_usage")
        system = cpu.get("system_cpu_usage")
        prev = self.prev_cpu.get(c.id)
        self.prev_cpu[c.id] = (total, system)
        if prev and None not in (total, system, *prev) and system > prev[1]:
            cores = cpu.get("online_cpus") or self.ncpu
            c.cpu = max(0.0, (total - prev[0]) / (system - prev[1]) * cores * 100)
        mem = s.get("memory_stats") or {}
        if "usage" in mem:
            extra = mem.get("stats") or {}
            c.mem = mem["usage"] - extra.get("inactive_file", extra.get("total_inactive_file", 0))

    def webs_for(self, c, config, published):
        explicit = parse_links(c.labels.get("zendock.web", ""))
        explicit += parse_links((config.get(c.name) or {}).get("web", []))
        if explicit:
            return explicit
        if c.state != "running":
            return []
        candidates = []
        for ip, hport, cport in c.ports:
            if hport not in NO_HTTP and cport not in NO_HTTP:
                host = ip if ip not in ("", "0.0.0.0", "::") else "127.0.0.1"
                candidates.append((host, hport))
        if c.net_mode == "host":
            candidates += [("127.0.0.1", p) for p in c.exposed
                           if p not in NO_HTTP and p not in published]
        # Sin respuesta se reintenta: cada 3 s en los dos primeros minutos, luego cada minuto
        retry_s = 3 if time.time() - c.started < 120 else 60
        webs = []
        for host, port in candidates:
            key = (c.id, c.started, port)
            url, definitive, checked = self.probes.get(key, (None, False, -1e9))
            if not definitive and time.monotonic() - checked > retry_s:
                self.pending.append((key, host, port))
            if url:
                webs.append((f"Web :{port}", url))
        return webs


class Events(QThread):
    """Flujo /events de Docker: despierta al sondeo y avisa de caidas."""

    notice = pyqtSignal(str, str)

    def __init__(self, docker, wake):
        super().__init__()
        self.docker = docker
        self.wake = wake
        self.stopping = False
        self.conn = None

    def run(self):
        stopped_at, oom_at, health = {}, {}, {}
        filters = urllib.parse.quote(json.dumps({"type": ["container"]}))
        while not self.stopping:
            try:
                self.conn, resp = self.docker.stream(f"/events?filters={filters}")
                self.wake.set()
                while not self.stopping:
                    line = resp.readline()
                    if not line:
                        break
                    ev = json.loads(line)
                    action = ev.get("Action") or ""
                    kind = action.split(":")[0]
                    if kind.startswith("exec_") or kind in ("top", "attach", "resize", "archive-path"):
                        continue        # los healthchecks generan exec_* cada pocos segundos
                    actor = ev.get("Actor") or {}
                    cid, attrs = actor.get("ID", ""), actor.get("Attributes") or {}
                    name, now = attrs.get("name", "?"), time.monotonic()
                    if kind in ("kill", "stop"):
                        stopped_at[cid] = now
                    elif kind == "oom":
                        oom_at[cid] = now
                        self.notice.emit(f"«{name}» se quedó sin memoria",
                                         "El kernel lo ha matado (OOM).")
                    elif kind == "die":
                        code = attrs.get("exitCode", "0")
                        on_purpose = now - stopped_at.get(cid, -1e9) < INTENCIONADO_S
                        if code != "0" and not on_purpose and now - oom_at.get(cid, -1e9) > 5:
                            self.notice.emit(f"Se ha caído «{name}»",
                                             f"El contenedor terminó con código {code}.")
                    elif kind == "health_status":
                        status = action.partition(":")[2].strip()
                        if status == "unhealthy" and health.get(cid) != "unhealthy":
                            self.notice.emit(f"«{name}» no está sano", "Su healthcheck está fallando.")
                        health[cid] = status
                    self.wake.set()
            except (DockerError, OSError, ValueError, http.client.HTTPException):
                pass
            finally:
                if self.conn is not None:
                    self.conn.close()
            for _ in range(30):     # reintento en 3 s, atento a la salida
                if self.stopping:
                    break
                time.sleep(0.1)

    def stop(self):
        self.stopping = True
        try:
            if self.conn is not None and self.conn.sock is not None:
                self.conn.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass


# ------------------------------------------------------------------ acciones
VERBOS = {"start": "Encendiendo…", "stop": "Apagando…", "restart": "Reiniciando…",
          "pause": "Pausando…", "unpause": "Reanudando…"}


class Worker(QObject):
    """Acciones lentas (apagar tarda hasta 10 s) fuera del hilo de la interfaz."""

    done = pyqtSignal(str, str)     # clave, error ("" si fue bien)

    def __init__(self, docker):
        super().__init__()
        self.docker = docker
        self.pool = ThreadPoolExecutor(max_workers=4, thread_name_prefix="zendock")

    def _submit(self, key, fn):
        def run():
            try:
                fn()
                err = ""
            except DockerError as exc:
                err = str(exc)
            except (OSError, subprocess.SubprocessError) as exc:
                err = str(exc)
            self.done.emit(key, err)
        self.pool.submit(run)

    def container(self, cid, verb):
        query = "?t=10" if verb in ("stop", "restart") else ""
        self._submit(cid, lambda: self.docker.call("POST", f"/containers/{cid}/{verb}{query}",
                                                   timeout=90))

    def project(self, group, verb):
        """Con compose si sus ficheros siguen ahi (respeta depends_on); si no, uno a uno."""
        def run():
            files = [f for f in group.config_files if os.path.exists(f)]
            if files and shutil.which("docker"):
                cmd = ["docker", "compose", "-p", group.project, "--project-directory", group.workdir]
                for f in files:
                    cmd += ["-f", f]
                res = subprocess.run(cmd + [verb], capture_output=True, text=True, timeout=300)
                if res.returncode:
                    lines = [l for l in res.stderr.strip().splitlines() if l.strip()]
                    raise DockerError(lines[-1] if lines else f"docker compose {verb} falló")
                return
            query = "?t=10" if verb in ("stop", "restart") else ""
            for c in group.ctrs:
                if (verb == "start") != c.alive:
                    self.docker.call("POST", f"/containers/{c.id}/{verb}{query}", timeout=90)
        self._submit(f"p:{group.project}", run)


def launch_app(app):
    if Path(app.path).parent in APP_DIRS and shutil.which("gtk-launch"):
        return QProcess.startDetached("gtk-launch", [app.id])[0]
    try:
        parser = configparser.RawConfigParser(strict=False, interpolation=None)
        parser.read(app.path, encoding="utf-8")
        args = [a for a in shlex.split(parser["Desktop Entry"].get("Exec", ""))
                if not re.fullmatch(r"%[a-zA-Z]", a)]
    except (configparser.Error, KeyError, ValueError):
        return False
    return bool(args) and QProcess.startDetached(args[0], args[1:])[0]


def in_terminal(title, args):
    if shutil.which("konsole"):
        return QProcess.startDetached("konsole", ["-p", f"tabtitle={title}", "--hold", "-e", *args])[0]
    for term in ("x-terminal-emulator", "kgx", "gnome-terminal", "xterm"):
        if shutil.which(term):
            return QProcess.startDetached(term, ["-e", *args])[0]
    return False


# ------------------------------------------------------------------ interfaz
def theme_icon(*names):
    for name in names:
        ic = QIcon.fromTheme(name)
        if not ic.isNull():
            return ic
    return QIcon()


def app_icon(app):
    if app.icon and os.path.isabs(app.icon) and os.path.exists(app.icon):
        return QIcon(app.icon)
    return theme_icon(app.icon, "application-x-executable")


def dock_icon(color, size=128):
    """Tres contenedores apilados, dibujados a mano para no depender del tema."""
    pm = QPixmap(size, size)
    pm.fill(Qt.GlobalColor.transparent)
    p = QPainter(pm)
    p.setRenderHint(QPainter.RenderHint.Antialiasing)
    p.scale(size / 100.0, size / 100.0)
    path = QPainterPath()
    path.setFillRule(Qt.FillRule.OddEvenFill)
    for x, y in ((8, 52), (52, 52), (30, 18)):
        path.addRoundedRect(QRectF(x, y, 40, 30), 4, 4)
        for i in range(3):          # nervios de la chapa, huecos en el relleno
            path.addRoundedRect(QRectF(x + 8 + i * 10, y + 7, 4, 16), 1.5, 1.5)
    p.setPen(Qt.PenStyle.NoPen)
    p.setBrush(color)
    p.drawPath(path)
    p.end()
    return QIcon(pm)


class Stat(QFrame):
    """Celda con una medida."""

    def __init__(self, title, parent=None):
        super().__init__(parent)
        self.setObjectName("stat")
        lay = QVBoxLayout(self)
        lay.setContentsMargins(10, 8, 10, 9)
        lay.setSpacing(1)
        self.key = QLabel(title)
        self.key.setObjectName("statKey")
        self.val = QLabel("--")
        self.val.setObjectName("statVal")
        lay.addWidget(self.key)
        lay.addWidget(self.val)

    def set(self, text):
        self.val.setText(text)


class Bar(QWidget):
    """Barra de progreso fina, pintada a mano para seguir el color de acento."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setFixedHeight(5)
        self.frac = 0.0
        self.accent = BLUE

    def set_frac(self, frac, accent):
        self.frac = max(0.0, min(1.0, frac or 0.0))
        self.accent = accent
        self.update()

    def paintEvent(self, _event):
        p = QPainter(self)
        p.setRenderHint(QPainter.RenderHint.Antialiasing)
        r = self.height() / 2
        track = QColor(self.palette().text().color())
        track.setAlpha(30)
        p.setPen(Qt.PenStyle.NoPen)
        p.setBrush(track)
        p.drawRoundedRect(QRectF(0, 0, self.width(), self.height()), r, r)
        if self.frac > 0.004:
            p.setBrush(self.accent)
            p.drawRoundedRect(QRectF(0, 0, self.width() * self.frac, self.height()), r, r)


class Meter(QFrame):
    """Celda con etiqueta, lectura y barra de ocupacion."""

    def __init__(self, title, parent=None):
        super().__init__(parent)
        self.setObjectName("stat")
        lay = QVBoxLayout(self)
        lay.setContentsMargins(10, 7, 10, 8)
        lay.setSpacing(5)
        top = QHBoxLayout()
        top.setSpacing(6)
        self.key = QLabel(title)
        self.key.setObjectName("statKey")
        self.sub = QLabel("")
        self.sub.setObjectName("meterSub")
        self.val = QLabel("--")
        self.val.setObjectName("meterVal")
        top.addWidget(self.key)
        top.addWidget(self.sub)
        top.addStretch(1)
        top.addWidget(self.val)
        lay.addLayout(top)
        self.bar = Bar()
        lay.addWidget(self.bar)

    def set(self, text, frac, accent=BLUE, sub=""):
        self.val.setText(text)
        self.sub.setText(sub)
        self.bar.set_frac(frac, accent)


class Dot(QWidget):
    def __init__(self, parent=None):
        super().__init__(parent)
        self.setFixedSize(10, 10)
        self.color = GREY

    def set_color(self, color):
        if color != self.color:
            self.color = color
            self.update()

    def paintEvent(self, _event):
        p = QPainter(self)
        p.setRenderHint(QPainter.RenderHint.Antialiasing)
        p.setPen(Qt.PenStyle.NoPen)
        p.setBrush(self.color)
        p.drawEllipse(QRectF(1, 1, 8, 8))


class Elided(QLabel):
    """Etiqueta de una linea que se recorta con '…' en vez de ensanchar la ventana."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.full = ""
        self.setSizePolicy(QSizePolicy.Policy.Ignored, QSizePolicy.Policy.Preferred)
        self.setMinimumWidth(40)

    def set_full(self, text):
        if text != self.full:
            self.full = text
            self._elide()

    def resizeEvent(self, event):
        super().resizeEvent(event)
        self._elide()

    def _elide(self):
        self.setText(self.fontMetrics().elidedText(self.full, Qt.TextElideMode.ElideRight,
                                                   max(self.width(), 20)))


def tool_button(icon_names, fallback, tip):
    b = QToolButton()
    b.setObjectName("tool")
    ic = theme_icon(*icon_names)
    if ic.isNull():
        b.setText(fallback)
    else:
        b.setIcon(ic)
    b.setIconSize(QSize(16, 16))
    b.setToolTip(tip)
    b.setCursor(Qt.CursorShape.PointingHandCursor)
    return b


def action_button(text, icon):
    b = QPushButton(text)
    b.setObjectName("action")
    b.setIcon(icon)
    b.setIconSize(QSize(14, 14))
    b.setCursor(Qt.CursorShape.PointingHandCursor)
    return b


def fill_web_button(btn, webs, enabled):
    """Un enlace abre directo; varios despliegan un menu."""
    btn.setVisible(bool(webs))
    if not webs:
        return
    btn.setEnabled(enabled)
    btn.setToolTip("\n".join(url for _n, url in webs) if enabled else "Enciéndelo para abrir su web")
    if len(webs) == 1:
        btn.setMenu(None)
        btn.setProperty("url", webs[0][1])
    else:
        menu = QMenu(btn)
        for name, url in webs:
            menu.addAction(f"{name}  ·  {url}").triggered.connect(
                lambda _c, u=url: QDesktopServices.openUrl(QUrl(u)))
        btn.setMenu(menu)
        btn.setProperty("url", None)


def open_web_button(btn):
    if btn.property("url"):
        QDesktopServices.openUrl(QUrl(btn.property("url")))


class Card(QFrame):
    """Un contenedor: estado, consumo y sus botones."""

    def __init__(self, win):
        super().__init__()
        self.setObjectName("stat")
        self.win = win
        self.c = None
        lay = QVBoxLayout(self)
        lay.setContentsMargins(10, 7, 6, 6)
        lay.setSpacing(2)

        top = QHBoxLayout()
        top.setSpacing(7)
        self.dot = Dot()
        self.name = QLabel()
        self.name.setObjectName("cardName")
        self.usage = QLabel()
        self.usage.setObjectName("meterSub")
        top.addWidget(self.dot)
        top.addWidget(self.name)
        top.addStretch(1)
        top.addWidget(self.usage)
        top.addSpacing(4)
        lay.addLayout(top)

        bottom = QHBoxLayout()
        bottom.setSpacing(3)
        self.sub = Elided()
        self.sub.setObjectName("cardSub")
        bottom.addSpacing(17)
        bottom.addWidget(self.sub, 1)
        self.web = action_button("Web", theme_icon("internet-web-browser", "globe", "applications-internet"))
        self.web.clicked.connect(lambda: open_web_button(self.web))
        self.app = action_button("", QIcon())
        self.app.clicked.connect(lambda: self.c and self.c.apps and win.launch(self.c.apps[0]))
        self.power = tool_button(("media-playback-start",), "▶", "Encender")
        self.power.clicked.connect(self.toggle_power)
        self.restart = tool_button(("view-refresh",), "⟳", "Reiniciar")
        self.restart.clicked.connect(lambda: win.container_action(self.c, "restart"))
        self.more = tool_button(("overflow-menu", "application-menu", "open-menu"), "⋯", "Más")
        self.more.setPopupMode(QToolButton.ToolButtonPopupMode.InstantPopup)
        self.more_menu = QMenu(self.more)
        self.more_menu.aboutToShow.connect(self.build_more)
        self.more.setMenu(self.more_menu)
        for w in (self.web, self.app, self.power, self.restart, self.more):
            bottom.addWidget(w)
        lay.addLayout(bottom)

    def toggle_power(self):
        if self.c:
            self.win.container_action(self.c, "stop" if self.c.alive else "start")

    def build_more(self):
        m, c, win = self.more_menu, self.c, self.win
        m.clear()
        if c is None:
            return
        m.addAction(theme_icon("utilities-log-viewer", "text-x-log", "document-preview"),
                    "Ver logs").triggered.connect(lambda: win.logs(c))
        shell = m.addAction(theme_icon("utilities-terminal"), "Abrir terminal dentro")
        shell.setEnabled(c.state == "running")
        shell.triggered.connect(lambda: win.shell(c))
        if c.state in ("running", "paused"):
            paused = c.state == "paused"
            m.addAction(theme_icon("media-playback-start" if paused else "media-playback-pause"),
                        "Reanudar" if paused else "Pausar").triggered.connect(
                lambda: win.container_action(c, "unpause" if paused else "pause"))
        m.addSeparator()
        for net, ip in c.ips.items():
            m.addAction(theme_icon("edit-copy"), f"Copiar IP  {ip}  ({net})").triggered.connect(
                lambda _c, v=ip: QApplication.clipboard().setText(v))
        m.addAction(theme_icon("edit-copy"), "Copiar nombre").triggered.connect(
            lambda: QApplication.clipboard().setText(c.name))
        if c.workdir and os.path.isdir(c.workdir):
            m.addAction(theme_icon("folder-open"), "Abrir carpeta del proyecto").triggered.connect(
                lambda: QDesktopServices.openUrl(QUrl.fromLocalFile(c.workdir)))

    def update_from(self, c, busy):
        self.c = c
        color, text = status_of(c)
        self.dot.set_color(AMBER if busy else color)
        self.name.setText(c.name)
        self.name.setEnabled(c.alive)
        self.usage.setText(f"{'--' if c.cpu is None else f'{c.cpu:.1f} %'}  ·  {size(c.mem)}"
                           if c.state == "running" else "")
        if c.state == "running":
            if c.net_mode == "host":
                text += " · red del host"
            elif c.ports:
                text += " · " + " ".join(f":{hport}" for _ip, hport, _c in c.ports)
        self.sub.set_full(busy or text)

        fill_web_button(self.web, c.webs, c.state == "running")
        self.app.setVisible(bool(c.apps))
        if c.apps:
            self.app.setText(c.apps[0].name)
            self.app.setIcon(app_icon(c.apps[0]))
            self.app.setToolTip(f"Abrir {c.apps[0].name}")

        alive = c.alive
        self.power.setIcon(theme_icon("media-playback-stop" if alive else "media-playback-start"))
        if self.power.icon().isNull():
            self.power.setText("■" if alive else "▶")
        self.power.setToolTip("Apagar" if alive else "Encender")
        self.power.setEnabled(not busy)
        self.restart.setEnabled(not busy and c.state == "running")

        tip = [f"Imagen: {c.image}", f"ID: {c.id[:12]}"]
        if c.project:
            tip.append(f"Proyecto: {c.project} · servicio {c.service}")
        if c.ports:
            tip.append("Puertos: " + ", ".join(f"{ip or '0.0.0.0'}:{hp} → {cp}" for ip, hp, cp in c.ports))
        if c.ips:
            tip.append("IPs: " + ", ".join(f"{ip} ({net})" for net, ip in c.ips.items()))
        if c.restart_policy and c.restart_policy != "no":
            tip.append(f"Reinicio: {c.restart_policy} · {c.restarts} reinicios")
        self.setToolTip("\n".join(tip))


class Header(QWidget):
    """Cabecera de un proyecto de compose, con sus acciones de conjunto."""

    def __init__(self, win):
        super().__init__()
        self.win = win
        self.g = None
        lay = QHBoxLayout(self)
        lay.setContentsMargins(2, 8, 6, 0)
        lay.setSpacing(4)
        self.title = QLabel()
        self.title.setObjectName("section")
        self.count = QLabel()
        self.count.setObjectName("meterSub")
        lay.addWidget(self.title)
        lay.addSpacing(4)
        lay.addWidget(self.count)
        lay.addStretch(1)
        self.apps_row = QHBoxLayout()
        self.apps_row.setSpacing(3)
        lay.addLayout(self.apps_row)
        self.app_buttons = []
        self.start = tool_button(("media-playback-start",), "▶", "Encender todo el proyecto")
        self.start.clicked.connect(lambda: win.project_action(self.g, "start"))
        self.stop = tool_button(("media-playback-stop",), "■", "Apagar todo el proyecto")
        self.stop.clicked.connect(lambda: win.project_action(self.g, "stop"))
        self.restart = tool_button(("view-refresh",), "⟳", "Reiniciar todo el proyecto")
        self.restart.clicked.connect(lambda: win.project_action(self.g, "restart"))
        self.folder = tool_button(("folder-open",), "…", "Abrir la carpeta del proyecto")
        self.folder.clicked.connect(
            lambda: QDesktopServices.openUrl(QUrl.fromLocalFile(self.g.workdir)))
        for w in (self.start, self.stop, self.restart, self.folder):
            lay.addWidget(w)

    def update_from(self, g, apps, busy):
        self.g = g
        running = sum(c.state == "running" for c in g.ctrs)
        self.title.setText(g.title)
        self.count.setText(busy or f"{running}/{len(g.ctrs)} en marcha")
        is_project = bool(g.project)
        for w in (self.start, self.stop, self.restart):
            w.setVisible(is_project)
        self.start.setEnabled(not busy and any(not c.alive for c in g.ctrs))
        self.stop.setEnabled(not busy and any(c.alive for c in g.ctrs))
        self.restart.setEnabled(not busy and running > 0)
        self.folder.setVisible(is_project and os.path.isdir(g.workdir))

        ids = [a.id for a in apps]
        if ids != [b.property("app_id") for b in self.app_buttons]:
            for b in self.app_buttons:
                b.deleteLater()
            self.app_buttons = []
            for app in apps:
                b = action_button(app.name, app_icon(app))
                b.setProperty("app_id", app.id)
                b.setToolTip(f"Abrir {app.name}")
                b.clicked.connect(lambda _c, a=app: self.win.launch(a))
                self.apps_row.addWidget(b)
                self.app_buttons.append(b)


class ZenDock(QWidget):
    def __init__(self, docker):
        super().__init__()
        self.docker = docker
        self.settings = QSettings("zendock", "zendock")
        self.snap = None
        self.busy = {}          # clave (id o "p:proyecto") -> texto de la accion en curso
        self.cards, self.headers, self.order = {}, {}, []
        self.tray_accent = None
        self.setWindowTitle("ZenDock")
        self.setMinimumWidth(440)
        self.resize(self.settings.value("size", QSize(500, 760)))
        self.build()
        self.build_tray()

        self.worker = Worker(docker)
        self.worker.done.connect(self.on_done)
        self.poller = Poller(docker)
        self.poller.snapshot.connect(self.on_snapshot)
        self.poller.failed.connect(self.on_failed)
        self.events = Events(docker, self.poller.wake)
        self.events.notice.connect(self.on_notice)
        self.poller.start()
        self.events.start()

    # ------------------------------------------------------------ construccion
    def build(self):
        root = QVBoxLayout(self)
        root.setContentsMargins(16, 14, 16, 14)
        root.setSpacing(10)

        top = QHBoxLayout()
        top.setSpacing(8)
        self.s_run = Stat("En marcha")
        self.s_run.setMinimumWidth(96)
        self.m_cpu = Meter("CPU")
        self.m_ram = Meter("RAM")
        top.addWidget(self.s_run)
        top.addWidget(self.m_cpu, 1)
        top.addWidget(self.m_ram, 1)
        root.addLayout(top)

        bar = QHBoxLayout()
        bar.addWidget(self.section("Contenedores"))
        bar.addStretch(1)
        self.only_running = QPushButton("Solo en marcha")
        self.only_running.setObjectName("chip")
        self.only_running.setCheckable(True)
        self.only_running.setChecked(self.settings.value("solo_en_marcha", False, type=bool))
        self.only_running.toggled.connect(self.on_filter)
        bar.addWidget(self.only_running)
        root.addLayout(bar)

        self.scroll = QScrollArea()
        self.scroll.setObjectName("list")
        self.scroll.setWidgetResizable(True)
        self.scroll.setFrameShape(QFrame.Shape.NoFrame)
        self.scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        inner = QWidget()
        inner.setObjectName("listInner")
        self.list = QVBoxLayout(inner)
        self.list.setContentsMargins(0, 0, 4, 0)
        self.list.setSpacing(6)
        self.list.addStretch(1)
        self.scroll.setWidget(inner)
        root.addWidget(self.scroll, 1)

        self.note = QLabel("Conectando con Docker…")
        self.note.setObjectName("note")
        self.note.setWordWrap(True)
        root.addWidget(self.note)
        self.apply_style()

    def section(self, text):
        lab = QLabel(text)
        lab.setObjectName("section")
        return lab

    def apply_style(self):
        dark = self.palette().window().color().lightness() < 128
        card = "rgba(255,255,255,0.055)" if dark else "rgba(0,0,0,0.045)"
        edge = "rgba(255,255,255,0.09)" if dark else "rgba(0,0,0,0.08)"
        dim = "rgba(255,255,255,0.55)" if dark else "rgba(0,0,0,0.52)"
        hover = "rgba(255,255,255,0.10)" if dark else "rgba(0,0,0,0.08)"
        handle = "rgba(255,255,255,0.16)" if dark else "rgba(0,0,0,0.18)"
        self.setStyleSheet(f"""
            QFrame#stat {{ background: {card}; border: 1px solid {edge};
                           border-radius: 9px; }}
            QLabel#statKey {{ color: {dim}; font-size: 10px;
                              text-transform: uppercase; letter-spacing: .6px; }}
            QLabel#statVal {{ font-size: 15px; font-weight: 600; }}
            QLabel#meterVal {{ font-size: 13px; font-weight: 600; }}
            QLabel#meterSub {{ color: {dim}; font-size: 11px; }}
            QLabel#section {{ color: {dim}; font-size: 10px; font-weight: 700;
                              text-transform: uppercase; letter-spacing: 1.1px;
                              margin-top: 2px; }}
            QLabel#note {{ color: {dim}; font-size: 10px; }}
            QLabel#cardName {{ font-size: 13px; font-weight: 600; }}
            QLabel#cardSub {{ color: {dim}; font-size: 11px; }}
            QPushButton#action {{
                background: {card}; border: 1px solid {edge}; border-radius: 7px;
                padding: 3px 9px; font-size: 11px; }}
            QPushButton#action:hover {{ background: {hover}; }}
            QPushButton#action:disabled {{ color: {dim}; }}
            QPushButton#action::menu-indicator {{ width: 0; }}
            QToolButton#tool {{ background: transparent; border: 1px solid transparent;
                                border-radius: 7px; padding: 3px; }}
            QToolButton#tool:hover {{ background: {hover}; border-color: {edge}; }}
            QToolButton#tool::menu-indicator {{ image: none; width: 0; }}
            QPushButton#chip {{
                background: {card}; border: 1px solid {edge}; border-radius: 8px;
                padding: 3px 10px; font-size: 11px; }}
            QPushButton#chip:hover {{ background: {hover}; }}
            QPushButton#chip:checked {{
                background: {BLUE.name()}; border-color: {BLUE.name()};
                color: white; font-weight: 600; }}
            QScrollArea#list, QWidget#listInner {{ background: transparent; }}
            QScrollBar:vertical {{ background: transparent; width: 8px; margin: 0; }}
            QScrollBar::handle:vertical {{ background: {handle}; border-radius: 4px;
                                           min-height: 32px; }}
            QScrollBar::handle:vertical:hover {{ background: {dim}; }}
            QScrollBar::add-line:vertical, QScrollBar::sub-line:vertical {{ height: 0; }}
            QScrollBar::add-page:vertical, QScrollBar::sub-page:vertical {{
                background: transparent; }}
        """)

    def build_tray(self):
        self.tray = QSystemTrayIcon(dock_icon(GREY), self)
        self.tray_menu = QMenu()
        self.tray_menu.aboutToShow.connect(self.build_tray_menu)
        self.tray.setContextMenu(self.tray_menu)
        self.tray.activated.connect(self.on_tray_click)
        self.tray.setToolTip("ZenDock")
        self.tray.show()

    def build_tray_menu(self):
        m = self.tray_menu
        m.clear()
        if self.snap:
            for g in group_containers(self.snap.containers):
                running = sum(c.state == "running" for c in g.ctrs)
                sub = m.addMenu(dock_icon(BLUE if running else GREY, 32),
                                f"{g.title}   {running}/{len(g.ctrs)}")
                if g.project:
                    busy = f"p:{g.project}" in self.busy
                    for verb, label, enabled in (
                            ("start", "Encender todo", any(not c.alive for c in g.ctrs)),
                            ("stop", "Apagar todo", any(c.alive for c in g.ctrs)),
                            ("restart", "Reiniciar todo", running > 0)):
                        act = sub.addAction(label)
                        act.setEnabled(enabled and not busy)
                        act.triggered.connect(lambda _c, gg=g, v=verb: self.project_action(gg, v))
                else:
                    for c in g.ctrs:
                        act = sub.addAction(f"{'Apagar' if c.alive else 'Encender'} {c.name}")
                        act.setEnabled(c.id not in self.busy)
                        act.triggered.connect(
                            lambda _c, cc=c: self.container_action(cc, "stop" if cc.alive else "start"))
                apps = self.snap.project_apps.get(g.project, []) + [a for c in g.ctrs for a in c.apps]
                webs = [(c, name, url) for c in g.ctrs if c.state == "running" for name, url in c.webs]
                if apps or webs:
                    sub.addSeparator()
                for app in apps:
                    sub.addAction(app_icon(app), f"Abrir {app.name}").triggered.connect(
                        lambda _c, a=app: self.launch(a))
                for c, name, url in webs:
                    sub.addAction(theme_icon("internet-web-browser"), f"{c.name} · {name}").triggered.connect(
                        lambda _c, u=url: QDesktopServices.openUrl(QUrl(u)))
            m.addSeparator()
        notify = QAction("Avisar si un contenedor se cae", self, checkable=True)
        notify.setChecked(self.settings.value("avisos", True, type=bool))
        notify.toggled.connect(lambda on: self.settings.setValue("avisos", on))
        m.addAction(notify)
        m.addSeparator()
        m.addAction("Mostrar ventana").triggered.connect(self.show_window)
        m.addAction("Salir").triggered.connect(QApplication.quit)

    # ----------------------------------------------------------------- acciones
    def container_action(self, c, verb):
        if c is None or c.id in self.busy:
            return
        self.busy[c.id] = VERBOS[verb]
        self.worker.container(c.id, verb)
        self.refresh()

    def project_action(self, g, verb):
        if g is None or f"p:{g.project}" in self.busy:
            return
        key = f"p:{g.project}"
        self.busy[key] = VERBOS[verb]
        self.worker.project(g, verb)
        self.refresh()

    def on_done(self, key, err):
        self.busy.pop(key, None)
        if err:
            name = key[2:] if key.startswith("p:") else next(
                (c.name for c in (self.snap.containers if self.snap else []) if c.id == key), key[:12])
            self.show_note(f"{name}: {err}")
        self.poller.wake.set()
        self.refresh()

    def launch(self, app):
        if not launch_app(app):
            self.show_note(f"No se pudo abrir {app.name}.")

    def logs(self, c):
        if not in_terminal(f"logs {c.name}", ["docker", "logs", "-f", "--tail", "300", c.name]):
            self.show_note("No hay un terminal para enseñar los logs (instala konsole).")

    def shell(self, c):
        cmd = ["docker", "exec", "-it", c.name, "sh", "-c",
               "command -v bash >/dev/null && exec bash || exec sh"]
        if not in_terminal(c.name, cmd):
            self.show_note("No hay un terminal para abrir la shell (instala konsole).")

    def show_note(self, text):
        self.note.setText(text)
        self.note.show()

    def on_filter(self, on):
        self.settings.setValue("solo_en_marcha", on)
        self.refresh()

    def on_tray_click(self, reason):
        if reason == QSystemTrayIcon.ActivationReason.Trigger:
            self.hide() if self.isVisible() else self.show_window()

    def on_ipc(self, server):
        """Otra invocacion (el lanzador del menu) pide la ventana."""
        conn = server.nextPendingConnection()
        if conn is not None:
            conn.disconnected.connect(conn.deleteLater)
        if self.isVisible() and self.isActiveWindow():
            self.hide()
        else:
            self.show_window()

    def on_notice(self, title, text):
        if self.settings.value("avisos", True, type=bool):
            self.tray.showMessage(title, text, QSystemTrayIcon.MessageIcon.Warning, 8000)

    def show_window(self):
        self.show()
        self.raise_()
        self.activateWindow()

    def closeEvent(self, event):
        self.settings.setValue("size", self.size())
        if self.tray.isVisible():
            event.ignore()
            self.hide()
        else:
            event.accept()

    def shutdown(self):
        self.settings.setValue("size", self.size())
        self.poller.stop()
        self.events.stop()
        self.worker.pool.shutdown(wait=False, cancel_futures=True)
        self.poller.wait(2000)
        self.events.wait(2000)

    # ------------------------------------------------------------------ lectura
    def on_failed(self, err):
        self.show_note(f"No se puede hablar con Docker: {err}")
        self.s_run.set("--")
        self.set_tray(GREY, f"ZenDock\n{err}")

    def on_snapshot(self, snap):
        self.snap = snap
        if self.note.text().startswith(("Conectando", "No se puede hablar con Docker")):
            self.note.hide()
        error = load_config()["error"]
        if error:
            self.show_note(error)
        self.refresh()

    def refresh(self):
        snap = self.snap
        if snap is None:
            return
        hide_stopped = self.only_running.isChecked()
        order, live_cards, live_headers = [], set(), set()
        for g in group_containers(snap.containers):
            shown = [c for c in g.ctrs if not hide_stopped or c.alive or c.id in self.busy]
            if not shown and f"p:{g.project}" not in self.busy:
                continue
            header = self.headers.get(g.key) or self.headers.setdefault(g.key, Header(self))
            header.update_from(g, snap.project_apps.get(g.project, []), self.busy.get(f"p:{g.project}"))
            order.append(header)
            live_headers.add(g.key)
            for c in shown:
                card = self.cards.get(c.id) or self.cards.setdefault(c.id, Card(self))
                card.update_from(c, self.busy.get(c.id))
                order.append(card)
                live_cards.add(c.id)

        for store, live in ((self.cards, live_cards), (self.headers, live_headers)):
            for key in [k for k in store if k not in live]:
                widget = store.pop(key)
                self.list.removeWidget(widget)
                widget.deleteLater()
        if order != self.order:
            for w in self.order:
                if w in order:
                    self.list.removeWidget(w)
            for i, w in enumerate(order):
                self.list.insertWidget(i, w)
                w.show()
            self.order = order

        ctrs = snap.containers
        running = [c for c in ctrs if c.state == "running"]
        self.s_run.set(f"{len(running)} de {len(ctrs)}")
        cpu = sum(c.cpu or 0 for c in running) / snap.ncpu
        self.m_cpu.set(f"{cpu:.1f} %", cpu / 100, AMBER if cpu >= 85 else BLUE,
                       sub=f"de {snap.ncpu} hilos")
        mem = sum(c.mem or 0 for c in running)
        frac = mem / snap.mem_total if snap.mem_total else 0
        self.m_ram.set(size(mem), frac, AMBER if frac >= 0.85 else BLUE, sub=f"de {size(snap.mem_total)}")

        trouble = [c for c in ctrs if is_trouble(c)]
        accent = AMBER if trouble or self.busy else (BLUE if running else GREY)
        tip = f"ZenDock\n{len(running)} de {len(ctrs)} en marcha"
        for c in trouble:
            tip += f"\n⚠ {c.name}: {status_of(c)[1]}"
        self.set_tray(accent, tip)

    def set_tray(self, accent, tip):
        if accent != self.tray_accent:
            self.tray_accent = accent
            self.tray.setIcon(dock_icon(accent))
        self.tray.setToolTip(tip)


def main():
    app = QApplication(sys.argv)
    app.setApplicationName("ZenDock")
    app.setApplicationDisplayName("ZenDock")
    app.setDesktopFileName("zendock")
    app.setWindowIcon(dock_icon(BLUE))
    app.setQuitOnLastWindowClosed(False)

    # Instancia unica: el lanzador vuelve a ejecutar este script, asi que si ya
    # hay una copia corriendo le pedimos la ventana y salimos.
    ping = QLocalSocket()
    ping.connectToServer(IPC)
    if ping.waitForConnected(400):
        if "--tray" in sys.argv:      # el autoarranque no debe sacar la ventana
            return 0
        ping.write(b"toggle")
        ping.waitForBytesWritten(400)
        ping.disconnectFromServer()
        return 0

    QLocalServer.removeServer(IPC)      # socket huerfano de un cierre brusco
    server = QLocalServer()
    server.listen(IPC)

    win = ZenDock(Docker())
    server.newConnection.connect(lambda: win.on_ipc(server))
    app.aboutToQuit.connect(win.shutdown)
    if "--tray" not in sys.argv:
        win.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
