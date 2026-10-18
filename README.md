# arbibot-docker

Arbitraż spot OKX EEA ↔ Bybit EU w Dockerze. Rust.

## Uruchomienie

```bash
docker compose pull
docker compose up -d                       # skaner, warianty na sucho, raport
docker compose --profile live up -d live   # bot na żywo
docker compose logs -f live
```

Ustawienia bota na żywo są w `compose.yaml`: TIA/USDC, transakcja do 40 USDC, próg 40 bp, maker na OKX po 10 USDC z marżą od 20 bp, dzienny limit straty 2 USDC.

## Schemat transakcji

- `--sequential`: najpierw noga na giełdzie, której cena dłużej stoi (jej oferta zniknie pierwsza), druga dopiero po wypełnieniu, z limitem bez straty względem ceny pierwszej. Gdy pierwsza nie wejdzie, nic się nie dzieje.
- Salda wyrównują się same: kierunek, który je wyrównuje, ma niższy próg (`--restore-min-bps`, 30 bp), gdy przeciwny ma mniej niż połowę jego środków.
- `--trade-rebalance`: gdy kierunek jest zablokowany od 15 min, transakcja odwrotna kosztem do 45 bp, najwyżej raz na 10 min.

Warianty na sucho (`dry` i `ab-*`) różnią się od głównego jednym ustawieniem: kolejnością nóg, rebalansem, progiem, marżą makera, opóźnieniem albo parą. Usługa `report` porównuje je codziennie o 8:00 czasu polskiego i wysyła skrót na Telegram.

## Obraz i wersje

GitHub Actions buduje `ghcr.io/nellynelson24/arbibot-docker` z każdego tagu `vX.Y.Z` (obraz `X.Y.Z`, `X.Y`, `latest`) i z gałęzi `main` (obraz `main`). Nowa wersja:

```bash
git tag v1.0.1 && git push origin v1.0.1
```

Zmienne `compose.yaml`: `ARBIBOT_VERSION` (domyślnie `latest`), `ARBIBOT_DATA` (`./data`), `ARBIBOT_SECRETS` (`~/.arbibot/secrets`). Budowa lokalna: `docker build -t ghcr.io/nellynelson24/arbibot-docker:latest .`

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
