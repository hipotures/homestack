# HomeStack: plan migracji do Rust

**Data:** 2026-10-12
**Podstawa:** `docs/migration/api-mapping.md` (ID funkcji A1–I2, warianty 7.x i decyzje z sekcji 10) oraz `hipotures/homestack` `main` @ `c09636b`.
**Cel:** serwer usług w Rust, który rozmawia z Proxmoxem przez REST API, a potem klient w Rust. Python zostaje wzorcem zachowania do samego końca.
**Miejsce w repo:** ten plik jako `docs/migration/PLAN.md`. Workspace Rust jest w korzeniu repo `homestack` (`Cargo.toml`, `crates/`); Python przeniesiony do `python/` i usuwany w fazie 15.

---

## 1. Instrukcja dla agenta

Polecenie dla agenta brzmi zawsze tak samo: **„Wykonaj fazę N z `docs/migration/PLAN.md`”**.

### 1.1 Przebieg jednej fazy

1. **Przeczytaj** opis fazy, `docs/migration/decisions.md` i raporty poprzednich faz.
2. **Sprawdź bramki.** Jeśli faza wymaga decyzji, której nie ma w `decisions.md`, zatrzymaj się i zapytaj. Nie zgaduj.
3. **Przeanalizuj kod Pythona** dla funkcji z zakresu fazy. Zapisz w `docs/migration/phase-N.md` listę zachowań i niezmienników: każdy warunek odmowy, kolejność kroków, co jest sprawdzane ponownie przed wykonaniem, co jest sprzątane przy błędzie. Ze ścieżką do pliku i funkcji.
4. **Najpierw testy.** Przenieś testy Pythona z zakresu fazy do Rust (sekcja 2.1) i dopisz scenariusze dla symulatora (2.2) oraz testy zgodności (2.3). Uruchom je i pokaż, że nie przechodzą.
5. **Implementacja**, aż wszystkie testy przejdą.
6. **`scripts/check.sh`**: całość testów, nie tylko bieżącej fazy. Nic z poprzednich faz nie może się zepsuć.
7. **Testy na żywym klastrze** (2.4), jeśli faza je przewiduje i środowisko na to pozwala. Jeśli nie pozwala, zapisz to w raporcie jako niewykonane, a nie jako zaliczone.
8. **Aktualizacja** manifestu testów (2.1), OpenAPI, README i raportu fazy.
9. **Commit i push** na gałąź `rust-migration`, tag `rust-phase-N`. Raport na koniec rozmowy (1.3).

### 1.2 Zasady

- **Nie zmieniaj zachowania**, chyba że wynika to z decyzji w `decisions.md`. Każda świadoma różnica wobec Pythona trafia do raportu fazy z uzasadnieniem.
- **Nie usuwaj kodu Pythona** przed fazą końcową. W kodzie Pythona wolno dodawać tylko narzędzia testowe (faza 0).
- **Nie oznaczaj testu jako wykluczony** bez powodu w manifeście. „Trudny do przeniesienia” nie jest powodem.
- **Żadnych operacji zmieniających stan na klastrze** poza poolem testowym i zakresem VMID testowych (2.4). Skrypty muszą to wymuszać, nie tylko zakładać.
- **Sekrety** (token PVE, token klienta) tylko z plików albo zmiennych środowiska, nigdy w repo, logach ani wyjściu testów.

### 1.3 Raport po fazie

W `docs/migration/phase-N.md` i w odpowiedzi:

- co zostało przeniesione (ID funkcji),
- testy: przeniesione / nowe / wykluczone (z powodami) / zaliczone,
- zgodność z Pythonem: liczba scenariuszy i wynik,
- testy na żywym klastrze: wykonane / niewykonane i dlaczego,
- świadome różnice wobec Pythona,
- otwarte pytania na następną fazę.

### 1.4 Kiedy się zatrzymać

- brakująca decyzja z `decisions.md`,
- zachowanie Pythona wygląda na błąd (zgłoś, nie poprawiaj po cichu),
- test na żywym klastrze nie przechodzi albo wymagałby wyjścia poza pool testowy,
- brak dostępu do tokenu lub klastra potrzebnego do bramki fazy.

---

## 2. Strategia testów

Ręcznych testów nie ma. Wszystko uruchamia jeden skrypt:

```
scripts/check.sh            # L1–L3, bez klastra
scripts/check.sh --live     # dodatkowo L4 na prawdziwym klastrze
scripts/check.sh --phase N  # tylko testy oznaczone fazą N (do szybkiej pętli)
```

CI (GitHub Actions) uruchamia L1–L3 przy każdym pushu. L4 nigdy nie działa w CI.

### 2.1 L1: testy przeniesione 1:1

- Python ma 594 testy w `python/tests/`. Plik `parity/manifest.toml` wymienia każdy z nich (`tests/test_lifecycle.py::Klasa::test_x`) i przypisuje mu test w Rust, status (`todo`, `ported`, `excluded`) oraz fazę.
- Skrypt w `check.sh` sprawdza, że manifest obejmuje wszystkie testy Pythona i że każdy wpis `ported` wskazuje istniejący, zaliczony test Rust.
- `excluded` wymaga powodu, np. „dotyczy TUI”, „dotyczy Herdr, transport znika”.
- Testy Pythona sprawdzają dziś listę poleceń shella (`FakeSession.commands`). W Rust te same asercje dotyczą listy wywołań API w symulatorze, po przetłumaczeniu na wspólny słownik operacji (2.3).

### 2.2 L2: symulator Proxmoxa (`pve-sim`)

Lokalny serwer HTTPS udający API Proxmoxa, ze stanem w pamięci:

- **Model:** node’y (online/offline), VM z configiem i `digest`, storage z wolumenami i typami treści, pule, tagi, zadania z UPID i logiem, odpowiedzi guest agenta (skryptowane per VM).
- **Ładowanie stanu ze scenariusza** (`scenarios/*.toml`): stan początkowy klastra, config HomeStacka, żądanie, oczekiwany wynik.
- **Wstrzykiwanie błędów:** błąd przy N-tym wywołaniu, przerwanie zadania w połowie, „zgubione potwierdzenie” (zmiana wykonana, odpowiedź nie dociera), node przechodzący w offline.
- **Dziennik wywołań** w postaci wspólnego słownika operacji (2.3).
- **Linie postępu w logach zadań** skopiowane z formatów, które Python dziś parsuje (`CLONE_TRANSFER_RE`, `MIGRATION_*_RE`).
- **Uprawnienia tokenu:** symulator odrzuca wywołanie, jeśli token scenariusza nie ma wymaganego uprawnienia. Dzięki temu testy pilnują tabeli uprawnień z dokumentu mapowania.

### 2.3 L3: zgodność Python ↔ Rust

Python jest wyrocznią. Dla tego samego scenariusza obie implementacje muszą dać to samo.

- **Słownik operacji:** wspólny, kanoniczny opis operacji na Proxmoxie, np. `clone{src=100,dst=200,full=true,storage=s}`, `set_config{vmid=200,keys=[scsi1]}`, `guest_exec{vmid=200,cmd="hostname"}`. Plik `parity/ops.md`.
- **Po stronie Pythona (tylko narzędzia testowe, dodawane w fazie 0):**
  - `FixtureTransport`: odpowiada na `pvesh get …` ze stanu scenariusza, więc `homestack … --json` działa bez klastra,
  - `RecordingTransport`: zapisuje wykonane polecenia,
  - tłumacz poleceń `qm`/`pvesm`/`pvesh`/`vzdump`/`perl` na słownik operacji.
- **Porównania:**
  - **plany i odczyty:** JSON z `homestack <komenda> --json` (Python na `FixtureTransport`) kontra odpowiedź serwera Rust (na `pve-sim`) po normalizacji (mapa zmienionych nazw pól w `parity/normalize.toml`),
  - **operacje zmieniające stan:** sekwencja operacji z Pythona kontra sekwencja z `pve-sim`, w tej samej kolejności tam, gdzie kolejność ma znaczenie,
  - **pliki generowane:** snippety Cloud-Init i inne artefakty porównywane bajt w bajt (dopóki decyzja nie zmienia ich treści).
- **Świadome różnice** (np. `get-fsinfo` zamiast `exec df`) są zapisane w `normalize.toml` z odwołaniem do decyzji. Bez wpisu każda różnica to błąd.

### 2.4 L4: prawdziwy klaster

- **Odczyty:** dozwolone zawsze. Najlepszy test zgodności: `homestack status --json` (Python) i `GET /v1/status` (Rust) na tym samym klastrze w tej samej chwili, porównane po normalizacji.
- **Operacje zmieniające stan:** tylko w poolu `homestack-test`, w zakresie VMID `240–249` (mieści się w schemacie IP `prefix.<vmid>`), z nazwami `hst-*`, z osobnym tokenem, który ma uprawnienia tylko do tego poola. Skrypty odmawiają działania poza tym zakresem.
- **Uruchamianie:** tylko z `--live` i przy ustawionym `HOMESTACK_LIVE_TOKEN_FILE`. Testy zawsze sprzątają po sobie, także po błędzie.
- **Node’y offline** (np. ten z GPU): testy, które ich wymagają, oznaczają się jako pominięte z powodem, a nie jako zaliczone.

---

## 3. Struktura w Rust

Workspace Cargo w korzeniu repo, crate’y w `crates/` (potwierdzone w fazie 0, zob. `phase-0.md`):

| Crate | Zawartość |
|---|---|
| `hs-model` | config serwera, typy domenowe, DTO planów, wyników i błędów (serde) |
| `hs-pve` | typowany klient API Proxmoxa: token, TLS, failover, zadania (UPID), guest agent |
| `hs-core` | operacje HomeStacka: plany, maszyny stanów `create`/`refresh`/`migrate`, weryfikacje. Zależy od traitu `Pve`, więc działa i z prawdziwym klientem, i z symulatorem |
| `hs-server` | HTTP API (axum), uwierzytelnianie, zadania (`202` + `/v1/jobs`), WebSocket z postępem, wgrywanie configu |
| `pve-sim` | symulator Proxmoxa (2.2) |
| `hs-parity` | narzędzia do testów zgodności (2.3) |
| `hs-cli` | klient w Rust (od fazy 13) |

Zasady:

- fazy dziennika i stany zadań jako `enum` z obowiązkowym `match`, bez stanów zapisanych jako napisy,
- `clippy -D warnings`, `cargo fmt --check`, brak `unwrap()` poza testami,
- OpenAPI generowane z kodu i trzymane w repo (`docs/openapi.json`); test sprawdza, że jest aktualne,
- format błędów i zasady API jak w mypowers: `schema_version`, kod błędu, `request_id`, brak odbijania danych wejściowych.

---

## 4. Fazy

Każda faza kończy się zielonym `check.sh`, commitem, pushem i tagiem. „Bramka” to warunek zakończenia fazy.

### Faza 0: szkielet i infrastruktura testów

**Cel:** działający serwer, który łączy się z Proxmoxem, i komplet narzędzi do testowania kolejnych faz.

- Workspace z crate’ami z sekcji 3, CI, lint, `scripts/check.sh`.
- `hs-server`: `GET /health/live`, `GET /v1/pve/version` (przekazuje `GET /version` z Proxmoxa), uwierzytelnianie tokenem Bearer z pliku, format błędów, logi (`tracing`).
- `hs-pve`: klient z tokenem PVE, weryfikacja TLS (systemowe CA plus opcjonalne własne CA), lista node’ów do failoveru, `GET /version`, `GET /nodes`.
- `pve-sim`: `/version`, `/nodes`, model zadań, ładowanie scenariusza, dziennik operacji.
- Python (tylko narzędzia testowe): `FixtureTransport`, `RecordingTransport`, tłumacz poleceń na słownik operacji, format scenariuszy.
- `manifest.toml` wygenerowany dla wszystkich 594 testów ze statusem `todo` i przypisaną fazą. Testy TUI i Herdr od razu jako `excluded` z powodem.
- Obraz kontenera i instrukcja uruchomienia w LXC albo Dockerze (jedna instancja).

**Bramka:**
- `check.sh` zielony, CI zielone.
- Test zgodności „na sucho”: jeden scenariusz odczytu przechodzi przez oba harnessy (Python na `FixtureTransport`, Rust na `pve-sim`).
- `--live`: serwer w kontenerze odpowiada na `GET /v1/pve/version` z prawdziwego klastra, z poprawną weryfikacją certyfikatu `pveX.efez.net`, a failover działa po wyłączeniu pierwszego adresu z listy (symulowany zły adres).

### Faza 1: config serwera i możliwości

**Zakres:** F2, F3, T3; sekcje 8.1 i 8.2 dokumentu mapowania.

- Schemat configu serwera (podzbiór obecnego `config.toml`) i walidacja przeniesiona z `config.load_config`.
- Narzędzie `hs-config split`: zamienia obecny `config.toml` na `server.toml` i `client.toml`.
- `PUT /v1/config` z `If-Match`, `GET /v1/config`, `GET /v1/config/version` (SHA-256), zapis atomowy, przetrwanie restartu, nagłówek z oczekiwaną wersją configu przy żądaniach zmieniających stan (`409` przy niezgodności).
- Sekrety z plików (`*_FILE`), nigdy w odpowiedziach.
- Sonda uprawnień tokenu (`GET /access/permissions`) i `GET /v1/capabilities`: które operacje są dostępne i czego brakuje.

**Testy:** odpowiednie testy z `test_config.py`; korpus poprawnych i błędnych configów, na którym Python i Rust muszą dać ten sam wynik (przyjęty / odrzucony z tą samą klasą błędu).

**Bramka:** `--live`: wgranie configu, restart kontenera, config zostaje; `capabilities` zgadza się z faktycznymi uprawnieniami tokenu.

### Faza 2: odczyty

**Zakres:** A1–A8, D1, D3, E1, E3 (odczyt).

- `GET /v1/status`, `GET /v1/workspaces/{id|name}`, `GET /v1/discovery`, `GET /v1/networks`, `GET /v1/storage`, `GET /v1/orphans`, `GET /v1/gold/readiness` (część PVE).
- Guest agent tylko na poziomie `Audit`: `ping`, `get-fsinfo`, `get-host-name`, `network-get-interfaces`.

**Testy:** `test_status.py`, części odczytowe `test_proxmox.py`, `test_discovery.py`, `test_gold.py`, `test_alien_workspace.py`; zgodność planów i statusów na scenariuszach (node offline, brak home, zły serial, osierocone wolumeny, VM `homestack-alien`).

**Bramka:** `--live`: `scripts/live-compare-status.sh` porównuje Python `status --json` z Rust `/v1/status` i `status <VM>` dla każdej VM HomeStacka. Różnice tylko te z `normalize.toml`.

> Od tej fazy możesz używać serwera do podglądu stanu klastra.

### Faza 3: zadania, guest agent i model operacji

**Zakres:** T4, T5, T6, D2, E2; sekcja 8.4.

- Śledzenie zadań (UPID, status, log) i zamiana logu na zdarzenia postępu.
- Model operacji: plan z ID i skrótem, ponowna walidacja przed wykonaniem, `202` + `GET /v1/jobs/{id}`, WebSocket z `snapshot`, postępem i heartbeatem.
- Wrapper `agent/exec` + `exec-status` z limitami czasu.
- Biblioteka weryfikacji gościa (D2) w wariancie z decyzji 5 (`exec` albo `Audit` + `FileRead`).
- Kontrakt gościa Gold (E2), z zasadą: nigdy nie uruchamiać Gold do inspekcji.

**Testy:** symulator ze skryptowanymi odpowiedziami agenta i logami zadań; zgodność weryfikacji D2 z Pythonem (te same scenariusze błędów: zły hostname, brak `authorized_keys`, `sudo` obecne, dodatkowy użytkownik).

**Bramka:** `--live`: gotowość Gold (całość) zgodna z wynikiem Pythona; zadanie testowe (np. `POST status/start` i `shutdown` VM w poolu testowym) śledzone od UPID do końca.

### Bramka decyzyjna przed fazą 4

Muszą być zapisane w `decisions.md` (numery z sekcji 10 dokumentu mapowania):

1. strategia snippetów (7.1 A, B albo C),
2. nazwa roota (7.2),
3. miejsce dziennika `refresh` (7.4),
4. poziom uprawnień w gościu (decyzja 5).

Bez nich agent nie zaczyna fazy 4.

### Faza 4: Cloud-Init i klucze

**Zakres:** C1, C2, C5 (oraz C3, jeśli wybrany wariant go wymaga).

- Generator snippetów przeniesiony z `cloudinit.write_snippets` i zapis w wybranym wariancie.
- Wykrywanie i usuwanie starych snippetów, skan referencji `cicustom`.
- Klucze publiczne z Gold (config, fallback przez agenta).

**Testy:** testy snippetów i kluczy z `test_lifecycle.py`, `test_create_execution.py`, `test_proxmox.py` i `test_workspace_ssh.py`; pliki porównywane bajt w bajt z wyjściem Pythona dla tych samych danych. Przy wariancie 7.1 A treść się zmienia, więc zgodność bajtowa zostaje zastąpiona nowymi plikami wzorcowymi i testem rozruchu (bramka).

**Bramka:** `--live`: VM testowa z poola uruchamia się z wygenerowaną konfiguracją i ma właściwy hostname, IP i zamontowany home (ten test można zrobić dopiero razem z fazą 5, jeśli wariant wymaga `create`; wtedy przesuwa się do bramki fazy 5).

### Faza 4b: instalacja i deinstalacja w klastrze

**Cel:** wszystko, co HomeStack potrzebuje w Proxmoxie, a czego API serwera samo nie założy, robi jeden dedykowany skrypt. Do fazy 4 robiliśmy to ręcznie (`pveum`, `scp`, zob. `deploy.md` i próby w `decisions.md`). Python tego nie potrzebował (działa jako root przez SSH), więc nie ma tu odpowiednika w mapowaniu ani testów Pythona do przeniesienia.

**Skrypt** (`deploy/pve/homestack-pve.sh`, sam Bash i narzędzia PVE, bez zależności): właściciel kopiuje go na dowolny node i uruchamia jako root.

- **Polecenia:** `install`, `uninstall`, `status` (co jest, czego brakuje, co różni się od oczekiwanego). `install` i `uninstall` najpierw pokazują plan zmian i wykonują go po potwierdzeniu (`--yes` bez pytania); `--dry-run` tylko plan.
- **Rozpoznanie środowiska:** czy to node PVE (`pveversion`, wersja co najmniej 9.2) i czy jest w klastrze (`/etc/pve/corosync.conf`, lista nodów z `/etc/pve/nodes` i `pvecm nodes`). Root noda w klastrze dostaje się do pozostałych nodów bez dodatkowych kluczy (klucze klastra), więc skrypt sam obsługuje cały klaster.
- **Raz na klaster** (`/etc/pve` jest wspólne): użytkownik `homestack@pve`, role HomeStacka, pool `homestack` (Gold według decyzji 9), token serwera (sekret zapisany raz do pliku `0600` na nodzie, nigdy do logu), ACL zawężone do poola, storage HomeStacka, storage snippetów, bridge (`SDN.Use`) i Gold, `VM.Config.Disk` według decyzji 10, `Pool.Audit`. Opcjonalnie (`--test`) pool `homestack-test` i token testowy z tymi samymi zasadami.
- **Na każdym nodzie** (bez wspólnego storage snippetów): oba snippety vendor w katalogu storage snippetów, z sumą kontrolną. Nowa wersja snippetów to ten sam `install`. Node niedostępny jest zgłaszany, a ponowne uruchomienie dokańcza pracę.
- **Sprawdzenia przed zmianą:** storage snippetów ma typ treści `snippets` (jeśli nie, skrypt pyta o dopisanie, nie usuwając innych typów), tagi `homestack-*` nie są zablokowane przez `registered-tags` / `user-tag-access` dla tokenu, Gold istnieje i ma tag `homestack-gold` (skrypt Gold nie zmienia, poza poolem według decyzji 9).
- **Certyfikaty:** skrypt ich nie wystawia, tylko sprawdza (`status`, ostrzeżenie w `install`): certyfikat API każdego noda (`:8006`) musi być zaufany i wystawiony na nazwę, pod którą serwer łączy się z nodem (dziś Let's Encrypt przez ACME Proxmoxa z wtyczką DNS OVH, odnawiany przez sam Proxmox). Przy certyfikacie samopodpisanym skrypt podaje ścieżkę CA klastra (`/etc/pve/pve-root-ca.pem`) do `HOMESTACK_PVE_CA_FILE`. TLS samego serwera HomeStack (Caddy, ewentualnie ACME) to faza 11.
- **Idempotencja:** drugie `install` niczego nie zmienia. Skrypt przejmuje to, co założyliśmy ręcznie (użytkownik, role `HomeStackServerDisks`, `HomeStackServerGoldGuest`, `HomeStackTest*`, pool `homestack-test`, token testowy, snippety na pve2), i doprowadza do stanu docelowego, zgłaszając każdą zmianę.
- **`uninstall`:** usuwa tokeny, ACL, role, użytkownika, snippety ze wszystkich nodów i pusty pool. Nigdy nie usuwa VM, wolumenów ani Gold. Pool z VM-kami zostaje i jest zgłaszany z listą VMID; snippetów używanych jeszcze przez jakąś VM (`cicustom`) nie usuwa bez `--force`.
- **Gold** dalej przygotowuje właściciel (README, „Preparing a Gold VM”). Ta faza tylko dopisuje do opisu wymagania wariantu A (NetworkManager, `netplan`, `udevadm`, `python3` z PyYAML) i kontrolę gotowości serwera, która je sprawdza.

**Testy:** `shellcheck`; skrypt na atrapach `pveum`, `pvesh`, `pvecm`, `pvesm`, `ssh` w `PATH`, które zapisują wywołania i odtwarzają stan (instalacja na jednym nodzie i w klastrze trzech nodów, drugi `install` bez zmian, `uninstall` po `install` wraca do stanu zera, pool z VM zostaje, node niedostępny); `GET /v1/capabilities` po stronie serwera mówi, czego z instalacji brakuje.

**Bramka:** `--live` na klastrze, za zgodą właściciela: `install --test` przejmuje dzisiejszy stan testowy bez zmiany uprawnień poza zaplanowanymi, drugie uruchomienie nic nie zmienia, snippety są na wszystkich trzech nodach z tą samą sumą, test rozruchu z fazy 4 przechodzi na każdym nodzie Gold; `uninstall --test` usuwa część testową i zostawia produkcyjną. Instalacja produkcyjna (`install` bez `--test`) to bramka przed fazą 5.

Wymaga decyzji 9 i 10 (od nich zależą ACL).

### Bramka przed fazą 5: pool `homestack` i token produkcyjny

Od fazy 5 serwer zmienia stan klastra. Token HomeStacka nie może wtedy pozwalać na zmianę ani usunięcie VM spoza HomeStacka (np. desktopu). Tagi `homestack-*` są tylko umową, a nie zabezpieczeniem: granicę wyznaczają ACL Proxmoxa na poolu.

- **Pool `homestack`** (zakłada go właściciel albo agent za jego zgodą, jak pool testowy). Trafiają do niego nowe workspace’y: testowe dalej w `homestack-test`, pierwsze produkcyjne w `homestack`. Gold według decyzji 9.
- **Istniejące workspace’y (dziś 200–210) zostają na razie poza poolem.** Właściciel przeniesie je później sam. Do tego czasu serwer tylko je czyta, a zmiany robi Python.
- **Uprawnienia tokenu serwera do zmian tylko na `/pool/homestack`** (rola `HomeStack` z sekcji 5 dokumentu mapowania), plus storage HomeStacka, bridge (`SDN.Use`) i Gold (`VM.Clone`). Żadnych uprawnień do zmian na `/`, `/vms` ani na pojedynczych VM spoza poola. `VM.Config.Disk` na `/vms` (dodane po fazie 2 dla skanu osieroconych wolumenów) zostaje zawężone według decyzji 10. Do sprawdzania przynależności do poola token potrzebuje `Pool.Audit` na `/pool/homestack` (bez tego `cluster/resources` nie pokazuje pola `pool`, sprawdzone w fazie 3).
- **Serwer pilnuje tego także sam, nie tylko przez ACL:** `create` klonuje z `pool=homestack`. `destroy`, `refresh`, `resize`, `migrate` i `addkey` odmawiają przed planem dla VM spoza poola, nawet jeśli tag się zgadza. Odmowa ma własny kod błędu.
- **Test na żywym klastrze (bramka):** tokenem produkcyjnym próba zmiany i usunięcia VM spoza poola (VM testowa poza oboma poolami, z tagiem `homestack-ws`) kończy się `403` od Proxmoxa i odmową serwera. `GET /access/permissions` tokenu nie pokazuje `VM.Allocate`, `VM.Config.*` ani `VM.PowerMgmt` poza poolami HomeStacka.

Pool, role, token i ACL zakłada skrypt z fazy 4b (`install` bez `--test`), uruchomiony przez właściciela.

Bez poola, zawężonego tokenu i zapisanych decyzji 9 i 10 agent nie zaczyna fazy 5.

### Faza 5: `create` i `destroy`

**Zakres:** B1, B7 (część serwerowa); zdarzenia `workspace.created` i `workspace.destroyed`.

- Pełny przepływ `create` na nodzie Gold z planem, weryfikacją i sprzątaniem przy błędzie (VM niszczona tylko przed startem, jak w Pythonie). Nowa VM od razu w poolu (`pool=homestack`, w testach `homestack-test`).
- `destroy` z ponowną weryfikacją tożsamości home i sprzątaniem snippetów.
- Odpowiedzi zawierają dane potrzebne klientowi do efektów lokalnych (VMID, nazwa, IP, użytkownik, „klucz hosta zmieniony”).

**Testy:** `test_create_execution.py`, części `test_lifecycle.py`; zgodność sekwencji operacji; wstrzykiwanie błędu po każdym kroku `create` z porównaniem sprzątania z Pythonem.

**Bramka:** `--live`: `create` → weryfikacja → `destroy` dla VM `hst-*` w poolu testowym, dwa razy pod rząd, bez śladów (wolumeny, snippety, VMID).

### Faza 6: `refresh` i odzysk

**Zakres:** B3, B4, E3 (blokada `homestack-lock`).

- Na początku fazy skrypt `scripts/live-probe-import-from.sh` sprawdza na poolu testowym oba warianty `import-from` z dokumentu mapowania (B3 krok 2) i zapisuje wynik w `decisions.md`.
- Maszyna stanów dziennika jako `enum`, dziennik w wybranym miejscu.

**Testy:** części `refresh` z `test_lifecycle.py`, `test_refresh_lock.py`; wstrzyknięcie przerwy po każdej fazie dziennika i „zgubione potwierdzenie”, potem odzysk musi zakończyć się tym samym stanem co w Pythonie.

**Bramka:** `--live`: `create` → zapis pliku znacznika w home → `refresh` → znacznik jest, root jest nowy → przerwany `refresh` (zabicie serwera w trakcie) → odzysk → `destroy`.

### Faza 7: `resize`

**Zakres:** B5.

- Inspekcja gościa, powiększanie dysku i warstw (partycja, LUKS z plikiem klucza, LVM, ext4, Btrfs), powtórzenie z tym samym rozmiarem kończy przerwane powiększanie.
- LUKS z hasłem zgodnie z decyzją 6. Domyślnie serwer zwraca, że ten tryb obsługuje klient.

**Testy:** `test_resize*.py`, `test_resize_inspection.py`, `test_resize_layers.py`, `test_resize_luks.py`, `test_resize_auth.py` (część z plikiem klucza).

**Bramka:** `--live`: home VM testowej powiększony o 1G, system plików widzi nowy rozmiar.

### Faza 8: `migrate`

**Zakres:** B6.

**Testy:** części `migrate` z `test_lifecycle.py`; błąd po wyłączeniu VM kończy się jej startem na nodzie źródłowym, jak w Pythonie.

**Bramka:** `--live`: VM testowa przeniesiona na inny node i z powrotem, z zachowaniem stanu zasilania i home. Pominięte z powodem, jeśli drugi node jest offline.

### Faza 9: `addkey`

**Zakres:** C4.

- Restart tylko na wyraźne żądanie (osobne wywołanie), stan `pending_restart` w odpowiedzi.

**Testy:** `test_addkey.py` (część serwerowa; zapis `identity_files` i SSH configu to klient).

**Bramka:** `--live`: dodanie testowego klucza do VM testowej, restart, klucz jest w obu `authorized_keys`.

### Faza 10: `create` na innym nodzie

**Zakres:** B2 w wariancie z decyzji 3 (7.3).

**Testy:** `test_create_transfer.py` (walidacja i weryfikacja odtworzonej VM) plus nowe testy wybranego wariantu.

**Bramka:** `--live`: `create` → weryfikacja → `destroy` na nodzie innym niż Gold.

### Faza 11: koordynacja wielu instancji i wdrożenie

**Zakres:** sekcja 8.3 dokumentu mapowania.

- Blokada operacji w Proxmoxie (zgodnie z 7.4 B albo tagiem), `digest` przy każdym zapisie.
- Klient: pierwsza osiągalna instancja do operacji, ta sama instancja do śledzenia zadania.
- Wdrożenie trzech instancji (po jednej na node), TLS przed serwerem (Caddy jak w mypowers), sprawdzanie zgodności wersji configu między instancjami.

**Testy:** dwie instancje serwera na jednym `pve-sim`, równoległy `refresh` tej samej VM: jedna wygrywa, druga dostaje `409`, stan spójny.

**Bramka:** `--live`: trzy instancje, `GET /v1/config/version` zgodne; wyłączenie jednej nie przerywa odczytów z klienta.

### Faza 12: klient Python przez serwer (przejściowo)

**Cel:** codzienne używanie serwera, zanim powstanie klient w Rust.

- W Pythonie `transport.type = "server"`: operacje na Proxmoxie idą przez API serwera. Setup, repo i efekty lokalne (I1, I2) zostają w Pythonie i biorą dane workspace z `GET /v1/workspaces/{id}`.

**Testy:** te same komendy CLI dają ten sam JSON bezpośrednio i przez serwer.

**Bramka:** `--live`: pełny cykl `create` → `setup` → `repo` → `refresh` → `destroy` przez serwer na VM testowej.

> Tę fazę można wykonać wcześniej, zaraz po fazie 5, jeśli chcesz szybciej przejść na serwer.

### Faza 13: klient w Rust (CLI)

**Zakres:** komendy operacji na Proxmoxie przez serwer, plany i potwierdzenia, `--json` z tymi samymi kodami wyjścia, efekty lokalne I1 (SSH config, `known_hosts`, `identity_files`) i I2 (FIDO, sygnały dotyku).

**Testy:** zgodność wyjścia `--json` i kodów wyjścia z Pythonem; testy z `test_cli.py`, `test_workspace_ssh.py`, `test_fido.py`, `test_touch.py`.

### Faza 14: setup, repo i backup w Rust

**Zakres:** G1–G8, H1–H3 (po stronie klienta, zgodnie z 7.5 A). Dzielona na podfazy według grup: 14a Files i Credentials, 14b Environment, 14c Applications i structured config, 14d BK, 14e Herdr, 14f repo.

**Testy:** `test_setup*.py`, `test_repo.py`, `test_bk.py`, `test_setup_backup*.py`, `test_setup_herdr.py`; `setup_guest.py` (program wysyłany do gościa) może zostać w Pythonie, bo działa po stronie VM. Decyzja w raporcie 14a.

**Bramka (każda podfaza):** `--live`: ta sama selekcja setupu na dwóch świeżych VM testowych, raz z Pythona, raz z Rust, daje identyczny stan (pliki, sumy kontrolne, `setup.json`).

### Faza 15: wyłączenie Pythona

- Manifest testów bez wpisów `todo`.
- Python CLI usunięty albo zarchiwizowany, testy zgodności zastąpione plikami wzorcowymi zapisanymi z ostatniej wersji Pythona.
- README i dokumentacja tylko dla wersji Rust.

---

## 5. Pliki prowadzone przez agenta

| Plik | Zawartość |
|---|---|
| `docs/migration/PLAN.md` | ten plan |
| `docs/migration/api-mapping.md` | mapowanie funkcji HomeStacka na API Proxmoxa (dokument mapowania) |
| `docs/migration/decisions.md` | decyzje z sekcji 10 dokumentu mapowania i wyniki prób (np. `import-from`) |
| `docs/migration/phase-N.md` | analiza zachowań, raport fazy |
| `parity/manifest.toml` | mapa 594 testów Pythona na testy Rust |
| `parity/ops.md` | słownik operacji |
| `parity/normalize.toml` | dozwolone różnice z uzasadnieniem |
| `scenarios/` | scenariusze dla `pve-sim` i testów zgodności |
| `docs/openapi.json` | kontrakt API serwera |
