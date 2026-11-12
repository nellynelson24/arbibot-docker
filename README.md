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

- Gdy Bybit odrzuci zabezpieczenie makera (np. brak środków), bot domyka pozycję na OKX transakcją odwrotną, najwyżej 3 razy na dobę; przy kolejnym odrzuceniu albo nieudanym domknięciu zatrzymuje się. Zlecenia bez jasnego wyniku nie są domykane, tylko zatrzymują bota.
- Środki zarezerwowane dla makera nie są dostępne dla takera ani rebalansu: otwarte zlecenia maker blokują walutę na OKX i środki na zabezpieczenie na Bybit, a wypełnienia czekające na zabezpieczenie blokują je na Bybit.
- `--sequential`: najpierw noga na giełdzie, której cena dłużej stoi (jej oferta zniknie pierwsza), druga dopiero po wypełnieniu, z limitem bez straty względem ceny pierwszej. Gdy pierwsza nie wejdzie, nic się nie dzieje.
- Salda wyrównują się same: kierunek, który je wyrównuje, ma niższy próg (`--restore-min-bps`, 30 bp), gdy przeciwny ma mniej niż połowę jego środków.
- `--trade-rebalance`: gdy kierunek jest zablokowany od 15 min, transakcja odwrotna kosztem do 45 bp, najwyżej raz na 10 min. Wymaga, by kierunek odwrotny miał co najmniej 3 × minimalną transakcję (30 USDC). Nogi rebalansu idą po kolei: najpierw OKX, Bybit dopiero po jej wypełnieniu, z limitem z planu rebalansu. Komunikat o braku środków na Telegramie mówi, czy bot wyrówna salda sam, czy trzeba przelać ręcznie.
- `--skew-bps K` (opcja, A/B na sucho): przechył progów według sald — kierunek wyrównujący salda ma próg niższy o K × nierównowagę, rozjeżdżający wyższy; tak samo marża makera. Przechył nigdy nie przekracza ±K, także gdy zlecenia maker rezerwują więcej, niż jest na koncie. Salda wyrównują się przy okazji zarabiających transakcji.
- `--lazy-rebalance` (opcja, A/B na sucho): płatny rebalans handlem tylko po sygnale straconym z braku środków, najwyżej raz na 30 min i 4 razy na dobę.
- `--rebalance-strict` (opcja, A/B na sucho): reszta rebalansu handlem z limitem z planu zamiast 1%, a próba bez wypełnienia nie zużywa dobowego limitu (ponowienie po 60 s).
- `--hedge-while-paused` (opcja): ponawianie zabezpieczeń makera także w pauzie i po zatrzymaniu, najwyżej co 10 s. Bez niej bot po zatrzymaniu tylko raz pisze na Telegramie, ile monety zostało bez zabezpieczenia.
- `--hedge-leftovers` (opcja, tylko na żywo): na starcie bot porównuje wypełnienia makera z poprzedniej sesji z zabezpieczeniami oraz historię zleceń OKX z dziennikiem. Brakujące zabezpieczenie zawsze zgłasza na Telegramie, a z tą opcją także je wykonuje. Bez opcji sprawdź, czy nie zabezpieczyłeś tego ręcznie.
- Zlecenia maker: przy zamykaniu bot anuluje zlecenia i jeszcze przez 3 s zabezpiecza wypełnienia, które wtedy przychodzą. Co 30 s sprawdza zlecenia bez kwotowania (np. złożone mimo błędu odpowiedzi) i anuluje te, które wiszą na OKX.
- Taker: druga noga i reszta poniżej minimum giełdy zostają jako „niezamknięte” bez zlecenia i bez zatrzymania bota. Gdy jedna noga wejdzie, a druga skończy się błędem, wykonana noga jest księgowana po cenie rynku do wyniku i limitu straty.
- Odczyt sald jest ponawiany 3 razy co 2 s przed zatrzymaniem bota. Klucze do odczytu z prawem wypłat blokują start tak samo jak klucze handlowe. `/stop` działa tylko od właściciela (sprawdzany nadawca) i jest wykonywany także wtedy, gdy przyszedł w ciągu 5 min przed startem.
- `--auto-transfer` (domyślnie wyłączone): przelewy monety i USDC między giełdami z giełdy, której klucz ma prawo wypłat (Bybit EU go nie daje), tylko na adresy wpłat własnych kont; opis w [VPS.md](VPS.md).

Warianty na sucho (`dry` i `ab-*`) mają te same ustawienia co bot na żywo (maker 20 USDC, leniwy rebalans do 30 bp, przechył 15 bp) i różnią się jednym: `ab-arb-strict` z `--rebalance-strict`, `ab-arb-fees50` z prowizjami obu giełd o połowę niższymi (`--fee-scale 0.5`, tylko na sucho; pokazuje, ile dałyby niższe opłaty, np. poziom VIP albo inna giełda), `ab-ena`, `ab-strk` i `ab-s` to inne pary. Usługa `report` porównuje je codziennie o 8:00 czasu polskiego i wysyła skrót na Telegram.

Raport poleca tylko pary, które przechodzą filtry płynności na obu giełdach: obrót 24 h co najmniej 250 tys. USD, spread najwyżej 30 bp i różnica cen między giełdami najwyżej 100 bp (notowania z chwili raportu). Te same filtry stosuje wybór pary bez `--pair`.

Skaner symuluje makera (zlecenie na OKX z marżą 20 bp, zabezpieczenie na Bybit albo Bitvavo) na wszystkich parach. Prowizje Bitvavo zależą od rynku: na rynkach USDC 0,05% maker i taker, na pozostałych 0,15% / 0,25%. Raport pokazuje osobny ranking dla zabezpieczenia na Bitvavo, żeby sprawdzić, czy ta giełda opłaca się przed napisaniem jej obsługi w bocie. Raport układa pary po filtrach płynności według szacowanego zysku makera na dobę, porównuje symulację obecnej pary z wynikiem na żywo i radzi zmianę pary, gdy przewaga zwraca koszt zmiany (~0,4 USDC) w tydzień.

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
- `touch data/STOP` albo `/stop` na Telegramie wstrzymuje handel. Plik STOP obecny przy starcie zatrzymuje bota przed pierwszym zleceniem i zakupem inwentarza.
- Po błędzie zlecenia albo limicie straty bot sam tworzy `data/STOP`; komunikat podaje, które nogi zostały wykonane. Wznowienie: usuń plik i `docker compose restart live`.

## Raport

Raport na żywo pokazuje też próby zabezpieczeń makera: udane i chybione, wiek książki Bybit i czas od wypełnienia (p50/p90) oraz poślizg wobec ceny z książki, z wnioskiem, czy chybienia biorą się z opóźnionych danych Bybit, czy z ruchu ceny. `/stan` na Telegramie to lista: uwagi (zatrzymanie, brak środków z bieżącym kosztem rebalansu handlem wobec limitu), kapitał bota (moneta i waluta pary na kontach handlowych) z saldami obu giełd, wyniki od restartu (maker, taker, rebalans, zmiana ceny monety), salda poza botem (inne monety i konta funding, z wyceną giełdy) oraz zysk z arbitrażu z dziennika (dziś, 7 i 30 dni, od początku z rozbiciem na maker, taker i rebalanse, od zmiany pary) i rozliczenie kapitału od początku: na starcie + zmiany sald poza botem + zysk + zmiana cen = teraz. Zmiany sald poza botem to skoki kapitału między restartami bez ruchu ceny w czasie postoju (od 1.12.2 bot zapisuje salda i cenę w każdym wierszu stanu) i różnice od 1 USDC między saldami giełd a księgowaniem bota w trakcie pracy (bot pisze o nich na Telegramie). Przelew zrobiony przy działającym bocie przed wersją 1.12.0 dopisuje się ręcznie wierszem `{"type":"outside_correction","value":-10}` w dzienniku `data/live/<data>.jsonl` (kwota w walucie kwotowania, minus dla wypłaty); bot uwzględni go po restarcie. Wiadomość „Start” podaje ustawienia makera i takera, salda i to samo rozliczenie, a podsumowanie dnia wynik dnia i rozliczenie. Raport dzienny o 8:00 wysyła usługa `report`; `live` wysyła go tylko awaryjnie o 8:10, gdy `report` go nie wysłał. Wynik „Na żywo: razem” w raporcie obejmuje rebalanse. Puste próby takera nie liczą się jako transakcje, a wynik w bp liczy się od wypełnionej wartości. Restart nie zeruje dobowych limitów domknięć i rebalansów. Wiadomości na Telegram są ponawiane przy błędzie sieci i limicie Telegrama i przycinane do 4096 znaków; gdy polecenia odbiera inny proces z tym samym tokenem, bot pisze o tym raz i ponawia co minutę.

```bash
docker compose exec live bot report
```

## Struktura

- `bins/bot` — bot
- `bins/scanner` — skaner okazji
- `bins/latency`, `bins/pair-scan` — narzędzia pomocnicze
- `crates/account` — API kont
- `crates/venues` — giełdy i prowizje
