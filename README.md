# arbibot-docker

Arbitraż spot OKX EEA ↔ Bybit EU w Dockerze. Rust.

## Uruchomienie

```bash
docker compose build
docker compose up -d                       # skaner, warianty na sucho, raport
docker compose --profile live up -d live   # bot na żywo
docker compose logs -f live
```

Ustawienia bota na żywo są w `compose.yaml`: STRK/USDC, transakcja do 40 USDC, próg 40 bp, maker na OKX po 10 USDC z marżą od 20 bp, dzienny limit straty 2 USDC.

Warianty na sucho (`dry` i `ab-*`) różnią się jednym ustawieniem: progiem, marżą makera, opóźnieniem, rebalansem albo parą. Usługa `report` porównuje je codziennie o 8:00 czasu polskiego i wysyła skrót na Telegram.

## Klucze

Pliki w `~/.arbibot/secrets/`, opis w [VPS.md](VPS.md). Klucze bez prawa wypłat; z prawem wypłat bot nie wystartuje. Kontener działa jako UID 1000: `data/` i klucze muszą być dla niego czytelne.

## Zatrzymanie

- `docker compose stop live` kończy pracę i anuluje zlecenia maker.
- `touch data/STOP` albo `/stop` na Telegramie wstrzymuje handel.
- Po błędzie zlecenia albo limicie straty bot sam tworzy `data/STOP`. Wznowienie: usuń plik i `docker compose restart live`.

## Raport

```bash
docker compose exec live bot report
```

## Struktura

- `bins/bot` — bot
- `bins/scanner` — skaner okazji
- `bins/latency`, `bins/pair-scan` — narzędzia pomocnicze
- `crates/account` — API kont
- `crates/venues` — giełdy i prowizje
