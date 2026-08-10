#!/usr/bin/env python3
"""Validación visual reproducible para una ejecución de compare-revisions.sh.

Compara el output actual contra el PDF original (calidad) o contra el output de
la revisión baseline (regresión). Selecciona páginas uniformes, raster-heavy y
con firmas, renderiza con Poppler y guarda métricas/mapas/láminas. Nunca escribe
en el corpus original ni dentro del repo.

Ejemplos:
  scripts/compare-visuals.py ../gemapdf-internal-docs/benchmarks/<run-id>
  scripts/compare-visuals.py <run-dir> --against original --pages 1,98,last \
    --file "doc-C.pdf"
  scripts/compare-visuals.py <run-dir> --against baseline --max-changed-pct 0
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import hashlib
import json
import math
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageOps, ImageStat


@dataclass(frozen=True)
class PageMetrics:
    file: str
    page: int
    selection_reason: str
    reference_width: int
    reference_height: int
    candidate_width: int
    candidate_height: int
    changed_pixels: int
    changed_pct: float
    mean_abs_error: float
    rms_error: float
    psnr_db: float
    max_channel_error: int
    status: str


def die(message: str, code: int = 2) -> "NoReturn":
    print(f"ERROR: {message}", file=sys.stderr)
    raise SystemExit(code)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Renderiza y compara visualmente el output actual contra el PDF "
            "original o contra el output baseline."
        )
    )
    parser.add_argument("run_dir", type=Path, help="directorio de la ejecución")
    parser.add_argument(
        "--against",
        choices=("original", "baseline"),
        default="original",
        help="referencia visual (default: original)",
    )
    parser.add_argument(
        "--pages",
        default="auto",
        help="'auto' o páginas/rangos separados por coma (default: auto)",
    )
    parser.add_argument(
        "--sample-pages",
        type=int,
        default=5,
        help="muestras uniformes en modo auto (default: 5)",
    )
    parser.add_argument(
        "--raster-pages",
        type=int,
        default=3,
        help="páginas con mayor carga raster en modo auto (default: 3)",
    )
    parser.add_argument(
        "--max-signature-pages",
        type=int,
        default=20,
        help="máximo de páginas firmadas en modo auto (default: 20)",
    )
    parser.add_argument(
        "--file",
        action="append",
        dest="files",
        help="nombre exacto de PDF; puede repetirse (default: todos)",
    )
    parser.add_argument("--dpi", type=int, default=150, help="DPI de render (default: 150)")
    parser.add_argument(
        "--pixel-threshold",
        type=int,
        default=8,
        help="diferencia máxima por canal que se considera ruido (default: 8)",
    )
    parser.add_argument(
        "--max-changed-pct",
        type=float,
        help="sale con código 1 si alguna página supera este porcentaje",
    )
    parser.add_argument(
        "--min-psnr",
        type=float,
        help="sale con código 1 si alguna página queda bajo este PSNR (dB)",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help="salida explícita; debe quedar fuera del repositorio",
    )
    return parser.parse_args()


def command_output(command: list[str]) -> str:
    try:
        return subprocess.run(
            command,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        ).stdout
    except subprocess.CalledProcessError as exc:
        detail = exc.stderr.strip() or exc.stdout.strip() or f"código {exc.returncode}"
        die(f"falló {' '.join(command[:2])}: {detail}", 1)


def command_json(command: list[str]) -> dict:
    result = subprocess.run(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    # qpdf usa 3 para éxito con warnings estructurales recuperables.
    if result.returncode not in (0, 3):
        detail = result.stderr.strip() or f"código {result.returncode}"
        die(f"falló {' '.join(command[:2])}: {detail}", 1)
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        die(f"JSON inválido de {' '.join(command[:2])}: {exc}", 1)


def read_key_values(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            values[key] = value
    return values


def read_input_hashes(path: Path) -> dict[str, str]:
    hashes: dict[str, str] = {}
    if not path.is_file():
        return hashes
    for line in path.read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{64})  (.+)", line)
        if match:
            hashes[match.group(2)] = match.group(1).lower()
    return hashes


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def page_count(pdf: Path) -> int:
    output = command_output(["pdfinfo", str(pdf)])
    match = re.search(r"^Pages:\s+(\d+)\s*$", output, flags=re.MULTILINE)
    if not match:
        die(f"pdfinfo no reportó el número de páginas de {pdf}", 1)
    return int(match.group(1))


def parse_pages(spec: str, total: int) -> list[int]:
    pages: set[int] = set()
    for raw_token in spec.split(","):
        token = raw_token.strip().lower()
        if not token:
            continue
        if token == "last":
            pages.add(total)
            continue
        match = re.fullmatch(r"(\d+)(?:-(\d+|last))?", token)
        if not match:
            die(f"selector de páginas inválido: {raw_token!r}")
        start = int(match.group(1))
        end_token = match.group(2)
        end = total if end_token == "last" else int(end_token or start)
        if start < 1 or end < start or end > total:
            die(f"rango {raw_token!r} fuera de 1..{total}")
        pages.update(range(start, end + 1))
    if not pages:
        die("la selección de páginas está vacía")
    return sorted(pages)


def evenly_spaced(total: int, count: int) -> list[int]:
    count = min(max(count, 1), total)
    if count == 1:
        return [1]
    return sorted(
        {round(1 + index * (total - 1) / (count - 1)) for index in range(count)}
    )


def cap_evenly(values: list[int], limit: int) -> list[int]:
    values = sorted(set(values))
    if limit <= 0:
        return []
    if len(values) <= limit:
        return values
    indexes = evenly_spaced(len(values), limit)
    return [values[index - 1] for index in indexes]


def qpdf_page_signals(pdf: Path) -> tuple[dict[int, int], list[int]]:
    if shutil.which("qpdf") is None:
        return {}, []
    data = command_json(
        [
            "qpdf",
            "--json",
            "--json-key=pages",
            "--json-key=acroform",
            str(pdf),
        ]
    )
    raster_pixels: dict[int, int] = {}
    for page in data.get("pages", []):
        page_number = page.get("pageposfrom1")
        if not isinstance(page_number, int):
            continue
        pixels = 0
        for image in page.get("images", []):
            width = image.get("width")
            height = image.get("height")
            if isinstance(width, int) and isinstance(height, int):
                pixels += max(width, 0) * max(height, 0)
        raster_pixels[page_number] = pixels

    signature_pages: list[int] = []
    for field in data.get("acroform", {}).get("fields", []):
        if field.get("fieldtype") == "/Sig":
            page_number = field.get("pageposfrom1")
            if isinstance(page_number, int) and page_number > 0:
                signature_pages.append(page_number)
    return raster_pixels, sorted(set(signature_pages))


def auto_page_reasons(
    pdf: Path,
    total: int,
    sample_pages: int,
    raster_pages: int,
    max_signature_pages: int,
) -> dict[int, set[str]]:
    reasons: dict[int, set[str]] = {}
    for page in evenly_spaced(total, sample_pages):
        reasons.setdefault(page, set()).add("uniform")

    raster_pixels, signature_pages = qpdf_page_signals(pdf)
    ranked = sorted(raster_pixels, key=lambda page: (-raster_pixels[page], page))
    for page in ranked[:raster_pages]:
        if raster_pixels[page] > 0:
            reasons.setdefault(page, set()).add("raster-heavy")
    for page in cap_evenly(signature_pages, max_signature_pages):
        if page <= total:
            reasons.setdefault(page, set()).add("signature")
    return reasons


def safe_stem(filename: str) -> str:
    stem = Path(filename).stem
    cleaned = re.sub(r"[^A-Za-z0-9._-]+", "_", stem).strip("._")
    return cleaned or "documento"


def render_page(pdf: Path, page: int, dpi: int, output: Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    prefix = output.with_suffix("")
    command_output(
        [
            "pdftoppm",
            "-png",
            "-singlefile",
            "-r",
            str(dpi),
            "-f",
            str(page),
            "-l",
            str(page),
            str(pdf),
            str(prefix),
        ]
    )
    if not output.is_file():
        die(f"Poppler no generó {output}", 1)


def on_white_canvas(image: Image.Image, size: tuple[int, int]) -> Image.Image:
    rgb = image.convert("RGB")
    if rgb.size == size:
        return rgb
    canvas = Image.new("RGB", size, "white")
    canvas.paste(rgb, (0, 0))
    return canvas


def compare_page(
    filename: str,
    page: int,
    selection_reason: str,
    reference_png: Path,
    candidate_png: Path,
    reference_label: str,
    threshold: int,
    output_dir: Path,
) -> PageMetrics:
    with Image.open(reference_png) as reference_source, Image.open(candidate_png) as candidate_source:
        reference_size = reference_source.size
        candidate_size = candidate_source.size
        canvas_size = (
            max(reference_size[0], candidate_size[0]),
            max(reference_size[1], candidate_size[1]),
        )
        reference = on_white_canvas(reference_source, canvas_size)
        candidate = on_white_canvas(candidate_source, canvas_size)

    diff = ImageChops.difference(reference, candidate)
    channels = diff.split()
    max_diff = ImageChops.lighter(ImageChops.lighter(channels[0], channels[1]), channels[2])
    histogram = max_diff.histogram()
    pixels = canvas_size[0] * canvas_size[1]
    changed = sum(histogram[threshold + 1 :])
    changed_pct = changed * 100.0 / pixels if pixels else 0.0
    mean_abs_error = sum(value * count for value, count in enumerate(histogram)) / (
        pixels * 255.0
    )
    rms_channels = ImageStat.Stat(diff).rms
    rms_error = math.sqrt(sum(value * value for value in rms_channels) / len(rms_channels))
    psnr_db = math.inf if rms_error == 0 else 20 * math.log10(255.0 / rms_error)
    max_error = max(index for index, count in enumerate(histogram) if count)

    heatmap = ImageOps.colorize(ImageOps.autocontrast(max_diff), black="black", white="red")
    heatmap.save(output_dir / f"diff-page-{page:04d}.png")

    header_height = 28
    sheet = Image.new("RGB", (canvas_size[0] * 3, canvas_size[1] + header_height), "white")
    sheet.paste(reference, (0, header_height))
    sheet.paste(candidate, (canvas_size[0], header_height))
    sheet.paste(heatmap, (canvas_size[0] * 2, header_height))
    draw = ImageDraw.Draw(sheet)
    draw.text((8, 8), reference_label.upper(), fill="black")
    draw.text((canvas_size[0] + 8, 8), "CURRENT", fill="black")
    draw.text((canvas_size[0] * 2 + 8, 8), "DIFFERENCE", fill="black")
    sheet.save(output_dir / f"comparison-page-{page:04d}.png")

    if reference_size != candidate_size:
        status = "dimension-mismatch"
    elif changed == 0:
        status = "equal"
    else:
        status = "changed"

    return PageMetrics(
        file=filename,
        page=page,
        selection_reason=selection_reason,
        reference_width=reference_size[0],
        reference_height=reference_size[1],
        candidate_width=candidate_size[0],
        candidate_height=candidate_size[1],
        changed_pixels=changed,
        changed_pct=changed_pct,
        mean_abs_error=mean_abs_error,
        rms_error=rms_error,
        psnr_db=psnr_db,
        max_channel_error=max_error,
        status=status,
    )


def write_reports(
    output_dir: Path,
    run_dir: Path,
    against: str,
    dpi: int,
    threshold: int,
    rows: list[PageMetrics],
) -> None:
    fieldnames = list(PageMetrics.__dataclass_fields__)
    with (output_dir / "results.csv").open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(vars(row) for row in rows)

    changed = sum(row.status != "equal" for row in rows)
    dimension_mismatches = sum(row.status == "dimension-mismatch" for row in rows)
    worst = max(rows, key=lambda row: row.changed_pct)
    lines = [
        "# Comparación visual GemaPDF",
        "",
        f"- Ejecución fuente: `{run_dir}`",
        f"- Referencia: `{against}`",
        f"- Render: Poppler a {dpi} DPI",
        f"- Umbral de ruido por canal: {threshold}/255",
        f"- Páginas comparadas: {len(rows)}",
        f"- Páginas con diferencias: {changed}",
        f"- Diferencias de dimensiones: {dimension_mismatches}",
        (
            f"- Mayor diferencia: `{worst.file}`, página {worst.page} "
            f"({worst.changed_pct:.6f}% de píxeles)"
        ),
        "",
        "| Archivo | Página | Selección | Cambio píxeles | MAE | RMS | PSNR | Máx. error | Estado |",
        "|---|---:|:---|---:|---:|---:|---:|---:|:---|",
    ]
    for row in rows:
        escaped = row.file.replace("|", "\\|")
        lines.append(
            f"| {escaped} | {row.page} | {row.selection_reason} | "
            f"{row.changed_pct:.6f}% | {row.mean_abs_error:.8f} | "
            f"{row.rms_error:.4f} | {row.psnr_db:.2f} dB | "
            f"{row.max_channel_error} | {row.status} |"
        )
    lines.extend(
        [
            "",
            "Los PNG `comparison-page-*.png` muestran referencia, current y el mapa de",
            "diferencias. Las métricas sirven para localizar cambios; la aceptación de",
            "calidad en documentos con compresión con pérdida requiere revisión visual.",
            "Los PDF fuente no se copian ni se modifican.",
            "",
        ]
    )
    (output_dir / "SUMMARY.md").write_text("\n".join(lines), encoding="utf-8")


def main() -> int:
    args = parse_args()
    if shutil.which("pdftoppm") is None or shutil.which("pdfinfo") is None:
        die("se requieren pdftoppm y pdfinfo (Poppler)")
    if args.dpi < 36 or args.dpi > 600:
        die("--dpi debe estar entre 36 y 600")
    if args.pixel_threshold < 0 or args.pixel_threshold > 255:
        die("--pixel-threshold debe estar entre 0 y 255")
    if args.max_changed_pct is not None and not 0 <= args.max_changed_pct <= 100:
        die("--max-changed-pct debe estar entre 0 y 100")
    if args.min_psnr is not None and args.min_psnr < 0:
        die("--min-psnr no puede ser negativo")
    if args.sample_pages < 1 or args.raster_pages < 0 or args.max_signature_pages < 0:
        die("los conteos de selección automática no pueden ser negativos")

    script = Path(__file__).resolve()
    repo = script.parent.parent
    run_dir = args.run_dir.expanduser().resolve()
    baseline_dir = run_dir / "baseline"
    current_dir = run_dir / "current"
    if not current_dir.is_dir() or (args.against == "baseline" and not baseline_dir.is_dir()):
        die(f"{run_dir} no contiene los directorios requeridos")

    metadata_file = run_dir / "metadata" / "run.txt"
    if not metadata_file.is_file():
        die(f"falta metadata/run.txt en {run_dir}")
    metadata = read_key_values(metadata_file)
    corpus_value = metadata.get("corpus")
    corpus = Path(corpus_value).expanduser().resolve() if corpus_value else None
    input_hashes = read_input_hashes(run_dir / "metadata" / "inputs.sha256")
    if args.against == "original" and (corpus is None or not corpus.is_dir()):
        die("metadata/run.txt no apunta a un corpus original accesible")

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output_dir = (
        args.output_dir or run_dir / "visual" / args.against / stamp
    ).expanduser().resolve()
    if output_dir == repo or repo in output_dir.parents:
        die(f"la salida visual debe quedar fuera del repositorio: {repo}")
    if output_dir.exists():
        die(f"la carpeta de salida ya existe: {output_dir}")

    current_files = {path.name: path for path in current_dir.glob("*.pdf")}
    if args.against == "baseline":
        reference_files = {path.name: path for path in baseline_dir.glob("*.pdf")}
    else:
        assert corpus is not None
        reference_files = {path.name: corpus / path.name for path in current_dir.glob("*.pdf")}
    common = sorted(reference_files.keys() & current_files.keys())
    if args.files:
        requested = set(args.files)
        missing = sorted(requested - set(common))
        if missing:
            die(f"PDF sin pareja {args.against}/current: {', '.join(missing)}")
        common = [name for name in common if name in requested]
    if not common:
        die("no hay parejas PDF para comparar")

    output_dir.mkdir(parents=True)
    rows: list[PageMetrics] = []
    try:
        for filename in common:
            reference_pdf = reference_files[filename]
            candidate_pdf = current_files[filename]
            if not reference_pdf.is_file():
                die(f"no existe la referencia para {filename}: {reference_pdf}")
            if args.against == "original" and filename in input_hashes:
                actual_hash = sha256(reference_pdf)
                if actual_hash != input_hashes[filename]:
                    die(f"el original cambió desde el benchmark: {filename}", 1)

            reference_pages = page_count(reference_pdf)
            candidate_pages = page_count(candidate_pdf)
            if reference_pages != candidate_pages:
                die(
                    f"{filename}: {args.against} tiene {reference_pages} páginas y "
                    f"current {candidate_pages}",
                    1,
                )
            if args.pages == "auto":
                # Las señales se toman siempre del original cuando está
                # accesible: Flatten elimina los widgets de firma del output.
                selection_pdf = (
                    corpus / filename
                    if corpus is not None and (corpus / filename).is_file()
                    else reference_pdf
                )
                reasons = auto_page_reasons(
                    selection_pdf,
                    reference_pages,
                    args.sample_pages,
                    args.raster_pages,
                    args.max_signature_pages,
                )
            else:
                reasons = {
                    page: {"explicit"}
                    for page in parse_pages(args.pages, reference_pages)
                }
            document_dir = output_dir / safe_stem(filename)
            for page in sorted(reasons):
                reason = "+".join(sorted(reasons[page]))
                print(f"→ {filename} página {page}/{reference_pages} ({reason})")
                reference_png = document_dir / "reference" / f"page-{page:04d}.png"
                candidate_png = document_dir / "current" / f"page-{page:04d}.png"
                render_page(reference_pdf, page, args.dpi, reference_png)
                render_page(candidate_pdf, page, args.dpi, candidate_png)
                rows.append(
                    compare_page(
                        filename,
                        page,
                        reason,
                        reference_png,
                        candidate_png,
                        args.against,
                        args.pixel_threshold,
                        document_dir,
                    )
                )
        write_reports(
            output_dir,
            run_dir,
            args.against,
            args.dpi,
            args.pixel_threshold,
            rows,
        )
    except BaseException:
        (output_dir / "INCOMPLETE").write_text(
            "La comparación no terminó; revisar stderr.\n", encoding="utf-8"
        )
        raise

    print(f"✓ resumen: {output_dir / 'SUMMARY.md'}")
    if args.max_changed_pct is not None:
        exceeded = [row for row in rows if row.changed_pct > args.max_changed_pct]
        if exceeded:
            print(
                f"ERROR: {len(exceeded)} página(s) superaron "
                f"{args.max_changed_pct}% de cambio",
                file=sys.stderr,
            )
            return 1
    if args.min_psnr is not None:
        below = [row for row in rows if row.psnr_db < args.min_psnr]
        if below:
            print(
                f"ERROR: {len(below)} página(s) quedaron bajo {args.min_psnr} dB PSNR",
                file=sys.stderr,
            )
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
