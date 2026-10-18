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
- `--auto-transfer`: przelewy monety i USDC między giełdami, gdy na jednej zostaje mniej niż 40% poziomu docelowego (najwyżej 6 na dobę, 100 USDC na przelew), tylko na adresy wpłat własnych kont. Wymaga osobnych kluczy do wypłat, opis w [VPS.md](VPS.md). Ręcznie: `bot transfer --asset USDC --from bybit --plan`.

Warianty na sucho (`dry` i `ab-*`) różnią się od głównego jednym ustawieniem: kolejnością nóg, rebalansem, progiem, marżą makera, opóźnieniem albo parą. Usługa `report` porównuje je codziennie o 8:00 czasu polskiego i wysyła skrót na Telegram.

## Dane rynkowe

- Świeżość: ping co 2 s na obu giełdach; dane są świeże, gdy pong doszedł najpóźniej 3 s temu, a książka jest zgodna. Bez świeżości obu giełd nie ma sygnałów takera, a zlecenia maker są wycofywane.
- Zgodność książek: delty Bybit muszą mieć kolejne numery `u` (snapshot albo `u = 1` resetuje książkę, duplikat jest pomijany). Luka albo skrzyżowana książka (kupno ≥ sprzedaż) oznacza książkę jako niezgodną i wymusza nową subskrypcję. Licznik luk jest w statusie i w raporcie dziennym.

## Obraz i wersje

GitHub Actions buduje `ghcr.io/nellynelson24/arbibot-docker` z każdego tagu `vX.Y.Z` (obraz `X.Y.Z`, `X.Y`, `latest`) i z gałęzi `main` (obraz `main`). Nowa wersja:

```bash
git tag v1.2.1 && git push origin v1.2.1
```

Zmienne `compose.yaml`: `ARBIBOT_VERSION` (domyślnie `latest`), `ARBIBOT_DATA` (`./data`), `ARBIBOT_SECRETS` (`~/.arbibot/secrets`). Budowa lokalna: `docker build -t ghcr.io/nellynelson24/arbibot-docker:latest .`

## Klucze

Pliki w `~/.arbibot/secrets/`, opis w [VPS.md](VPS.md). Klucze do odczytu i handlu bez prawa wypłat (z prawem wypłat bot nie wystartuje); wypłaty tylko osobnymi kluczami `federbot-<giełda>-withdraw`. Kontener działa jako UID 1000: `data/` i klucze muszą być dla niego czytelne.

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
