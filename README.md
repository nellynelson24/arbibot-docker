# arbibot-docker

Arbitraż spot OKX EEA ↔ Bybit EU w Dockerze. Rust.

## Uruchomienie

```bash
docker compose pull
docker compose up -d                       # skaner, warianty na sucho, raport
docker compose --profile live up -d live   # bot na żywo
docker compose logs -f live
```

Ustawienia bota na żywo są w `compose.yaml`: para `ARBIBOT_LIVE_PAIR` (domyślnie ARB/USDC), transakcja do `ARBIBOT_SIZE` (domyślnie 40 USDC), próg 40 bp, maker na OKX po `ARBIBOT_MAKER_SIZE` (domyślnie 10 USDC) z marżą `ARBIBOT_MAKER_MIN_BPS` (domyślnie 20 bp), dzienny limit straty `ARBIBOT_DAILY_LOSS` (domyślnie 2 USDC).

Pierwszy start na nowej parze: `ARBIBOT_LIVE_FLAGS=--buy-inventory`. Bot kupuje monetę pary za USDC na giełdzie, gdzie stanowi mniej niż połowę wartości, jeśli brakuje co najmniej ćwierci transakcji (10 USDC). Inne monety na kontach (np. STRK) zostają nietknięte. Każdy zakup i salda obu giełd przy starcie idą na Telegram. Transakcja zmniejsza się do dostępnych środków, najmniej do ćwierci `--size`.

## Schemat transakcji

- `--sequential`: najpierw noga na giełdzie, której cena dłużej stoi (jej oferta zniknie pierwsza), druga dopiero po wypełnieniu, z limitem bez straty względem ceny pierwszej. Gdy pierwsza nie wejdzie, nic się nie dzieje.
- Salda wyrównują się same: kierunek, który je wyrównuje, ma niższy próg (`--restore-min-bps`, 30 bp), gdy przeciwny ma mniej niż połowę jego środków.
- `--trade-rebalance`: gdy kierunek jest zablokowany od 15 min, transakcja odwrotna kosztem do 45 bp, najwyżej raz na 10 min. Wymaga, by kierunek odwrotny miał co najmniej 3 × minimalną transakcję (30 USDC). Nogi rebalansu idą po kolei: najpierw OKX, Bybit dopiero po jej wypełnieniu, z limitem z planu rebalansu. Komunikat o braku środków na Telegramie mówi, czy bot wyrówna salda sam, czy trzeba przelać ręcznie.
- `--skew-bps K` (opcja, A/B na sucho): przechył progów według sald — kierunek wyrównujący salda ma próg niższy o K × nierównowagę, rozjeżdżający wyższy; tak samo marża makera. Salda wyrównują się przy okazji zarabiających transakcji.
- `--lazy-rebalance` (opcja, A/B na sucho): płatny rebalans handlem tylko po sygnale straconym z braku środków, najwyżej raz na 30 min i 4 razy na dobę.
- `--auto-transfer` (domyślnie wyłączone): przelewy monety i USDC między giełdami z giełdy, której klucz ma prawo wypłat (Bybit EU go nie daje), tylko na adresy wpłat własnych kont; opis w [VPS.md](VPS.md).

Warianty na sucho (`dry` i `ab-*`) różnią się od głównego (ARB/USDC, leniwy rebalans) jednym ustawieniem: `ab-arb` rebalans nie leniwy, `ab-arb-m30` marża makera 30 bp, `ab-arb-tani` rebalans tylko do 30 bp, `ab-arb-bez` bez płatnego rebalansu, `ab-arb-skew` przechył 15 bp, `ab-arb-maker20` zlecenie maker 20 USDC, `ab-strk` i `ab-s` inna para. Usługa `report` porównuje je codziennie o 8:00 czasu polskiego i wysyła skrót na Telegram.

Raport poleca tylko pary, które przechodzą filtry płynności na obu giełdach: obrót 24 h co najmniej 250 tys. USD, spread najwyżej 30 bp i różnica cen między giełdami najwyżej 100 bp (notowania z chwili raportu). Te same filtry stosuje wybór pary bez `--pair`.

Skaner symuluje makera (zlecenie na OKX z marżą 20 bp, zabezpieczenie na Bybit) na wszystkich parach. Raport układa pary po filtrach płynności według szacowanego zysku makera na dobę, porównuje symulację obecnej pary z wynikiem na żywo i radzi zmianę pary, gdy przewaga zwraca koszt zmiany (~0,4 USDC) w tydzień.

## Dane rynkowe

- Świeżość: ping co 2 s na obu giełdach; dane są świeże, gdy pong doszedł najpóźniej 3 s temu, a książka jest zgodna. Bez świeżości obu giełd nie ma sygnałów takera, a zlecenia maker są wycofywane.
- Zgodność książek: delty Bybit muszą mieć kolejne numery `u` (snapshot albo `u = 1` resetuje książkę, duplikat jest pomijany). Luka albo skrzyżowana książka (kupno ≥ sprzedaż) oznacza książkę jako niezgodną i wymusza nową subskrypcję. Licznik luk jest w statusie i w raporcie dziennym.

## Obraz i wersje

GitHub Actions buduje `ghcr.io/nellynelson24/arbibot-docker` z każdego tagu `vX.Y.Z` (obraz `X.Y.Z`, `X.Y`, `latest`) i z gałęzi `main` (obraz `main`). Nowa wersja:

```bash
git tag v1.5.0 && git push origin v1.5.0
```

Zmienne `compose.yaml`: `ARBIBOT_VERSION` (domyślnie `latest`), `ARBIBOT_DATA` (`./data`), `ARBIBOT_SECRETS` (`~/.arbibot/secrets`). Budowa lokalna: `docker build -t ghcr.io/nellynelson24/arbibot-docker:latest .`

## Klucze

Pliki w `~/.arbibot/secrets/`, opis w [VPS.md](VPS.md). Klucze do odczytu i handlu bez prawa wypłat (z prawem wypłat bot nie wystartuje); wypłaty tylko osobnymi kluczami `federbot-<giełda>-withdraw`. Kontener działa jako UID 1000: `data/` i klucze muszą być dla niego czytelne.

## Zatrzymanie

- `docker compose stop live` kończy pracę i anuluje zlecenia maker.
- `touch data/STOP` albo `/stop` na Telegramie wstrzymuje handel.
- Po błędzie zlecenia albo limicie straty bot sam tworzy `data/STOP`; komunikat podaje, które nogi zostały wykonane. Wznowienie: usuń plik i `docker compose restart live`.

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
