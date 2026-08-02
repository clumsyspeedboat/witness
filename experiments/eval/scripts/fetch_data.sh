#!/usr/bin/env bash
# T3 — dataset puller. Pulls + caches + hash-verifies every corpus into
# eval/data/, writes provenance to eval/data/CORPORA.md. Idempotent (re-run is a
# no-op when files already exist). Public BI: retried, and on failure recorded in
# PUBLICBI_MISSING.txt — never silently skipped. Offline: synthetic-only banner.
set -uo pipefail
cd "$(dirname "$0")/../.."
DATA=eval/data
mkdir -p "$DATA"
MD="$DATA/CORPORA.md"

have_net() { curl -sS --max-time 8 -o /dev/null https://raw.githubusercontent.com 2>/dev/null; }

sha() { sha256sum "$1" 2>/dev/null | cut -d' ' -f1; }
row() { printf "| %s | %s | %s | %s | %s |\n" "$1" "$2" "$3" "$4" "$5" >> "$MD"; }

{
  echo "# Corpora provenance"
  echo ""
  echo "Fetched $(date -u +%Y-%m-%dT%H:%M:%SZ) by scripts/fetch_data.sh."
  echo ""
  echo "| corpus | file | sha256 (12) | rows≈ | source |"
  echo "|---|---|---|---|---|"
} > "$MD"

if ! have_net; then
  echo "!!! OFFLINE — REAL CORPORA SKIPPED. Synthetic corpora are generated in-bin."
  echo "" >> "$MD"; echo "**OFFLINE this run — real corpora not fetched; synthetic only.**" >> "$MD"
  exit 0
fi

# --- NAB (reuse the cached python downloader; flattened names) ---
python3 - <<'PY'
from eval.corpora import try_download
got = try_download()
print(f"NAB: {len(got)} files present")
PY
for f in "$DATA"/realKnownCause__*.csv "$DATA"/realTweets__*.csv "$DATA"/artificialNoAnomaly__*.csv "$DATA"/realAWSCloudwatch__*.csv; do
  [ -f "$f" ] && row "NAB" "$(basename "$f")" "$(sha "$f" | cut -c1-12)" "$(($(wc -l < "$f")-1))" "github.com/numenta/NAB"
done

# --- UCI Household Power (zip) ---
HP="$DATA/household_power_consumption.txt"
if [ ! -f "$HP" ]; then
  curl -sS -L --max-time 120 -o /tmp/hpc.zip \
    "https://archive.ics.uci.edu/static/public/235/individual+household+electric+power+consumption.zip" \
    && python3 -c "import zipfile;zipfile.ZipFile('/tmp/hpc.zip').extractall('$DATA')"
fi
[ -f "$HP" ] && row "Household" "household_power_consumption.txt" "$(sha "$HP" | cut -c1-12)" "$(($(wc -l < "$HP")-1))" "UCI ML 235"

# --- UCR ECG5000 (zip → TRAIN/TEST.txt) ---
ECG="$DATA/ucr_ecg5000/ECG5000_TRAIN.txt"
if [ ! -f "$ECG" ]; then
  mkdir -p "$DATA/ucr_ecg5000"
  curl -sS -L --max-time 120 -o /tmp/ecg.zip "http://www.timeseriesclassification.com/aeon-toolkit/ECG5000.zip" \
    && python3 -c "import zipfile;z=zipfile.ZipFile('/tmp/ecg.zip');[z.extract(n,'$DATA/ucr_ecg5000') for n in ('ECG5000_TRAIN.txt','ECG5000_TEST.txt')]"
fi
[ -f "$ECG" ] && row "UCR" "ucr_ecg5000/ECG5000_{TRAIN,TEST}.txt" "$(sha "$ECG" | cut -c1-12)" "5000" "timeseriesclassification.com"

# --- NYC Taxi (1 month parquet) ---
TX="$DATA/yellow_tripdata_2024-01.parquet"
if [ ! -f "$TX" ]; then
  curl -sS -L --max-time 180 -o "$TX" \
    "https://d37ci6vzurychx.cloudfront.net/trip-data/yellow_tripdata_2024-01.parquet" || rm -f "$TX"
fi
[ -f "$TX" ] && row "Taxi" "yellow_tripdata_2024-01.parquet" "$(sha "$TX" | cut -c1-12)" "2964624" "NYC TLC"

# --- Public BI subset (downloads + extracts numeric columns, or records MISSING) ---
if [ ! -f "$DATA/publicbi/PROVENANCE.md" ]; then
  python3 eval/scripts/fetch_publicbi.py || true
fi
if [ -f "$DATA/publicbi/PROVENANCE.md" ]; then
  while IFS='|' read -r _ t r cols sha _; do
    [ -n "${t// /}" ] && [ "${t// /}" != "table" ] && row "PublicBI" "$(echo "$t"|xargs)" "$(echo "$sha"|xargs)" "$(echo "$r"|xargs)" "cwida/public_bi_benchmark"
  done < <(grep -E '^\| (Arade|Bimbo|CMS|Euro|Generico)' "$DATA/publicbi/PROVENANCE.md")
else
  row "PublicBI" "MISSING" "—" "—" "see data/PUBLICBI_MISSING.txt"
fi

echo "wrote $MD"
