# HomeStack → Proxmox VE API: mapowanie funkcji

**Data:** 2026-10-12
**Cel:** pierwszy krok do rozdzielenia HomeStacka na serwer usług (API) i cienkich klientów (CLI, TUI, web, widget). Dokument opisuje tylko funkcje. UI/TUI pomijam.
**Podstawa:**

- HomeStack: `hipotures/homestack`, `main` @ `c09636b` (2026-10-11). Mapowanie zrobione z kodu w `src/homestack/`, nie z menu TUI.
- Wzorzec serwera: `hipotures/mypowers` @ `296d7f7`.
- Proxmox: źródła `qemu-server 9.2.11`, `libpve-storage-perl 9.1.13`, `pve-manager 9.2.21`, `libpve-access-control` (uprawnienia `VM.GuestAgent.*` od 9.0.2).

**Czego nie sprawdzałem:** żywego klastra, prawdziwego configu ani zachowania storage. Endpointy i uprawnienia sprawdziłem w źródłach Proxmoxa. Przed implementacją trzeba potwierdzić wersję klastra (`GET /version`) i przetestować miejsca oznaczone „test”.

---

## 0. Legenda

Pokrycie przez API Proxmoxa:

- ✅ **API**: natywny REST 1:1. Większość wywołań to dziś `pvesh`, czyli to samo API wywołane lokalnie na nodzie.
- 🟢 **API+QGA**: REST plus guest agent (`/agent/*`). `agent/exec` daje roota w gościu (uprawnienie `VM.GuestAgent.Unrestricted`).
- 🟡 **Częściowo**: API pokrywa większość kroków, jeden lub dwa wymagają obejścia.
- ❌ **Brak**: w API nie ma odpowiednika. Potrzebne obejście (sekcja 7).
- 💻 **Klient**: efekt na desktopie albo wymaga kluczy sprzętowych lub `gh`. Nie przenosi się na serwer bez zmiany modelu zaufania.

Miejsce wykonania: **S** serwer, **K** klient, **S+K** podział.

---

## 1. Podsumowanie

| ID | Funkcja | Pokrycie | Gdzie | Główna luka / uwaga |
|---|---|---|---|---|
| A1 | `status` globalny | ✅ | S | zajętość home przez `get-fsinfo` (D3) |
| A2 | `status VM` | 🟢 | S | da się zejść do `VM.GuestAgent.Audit` |
| A3 | rozpoznanie celu (nazwa → VMID) | ✅ | S | — |
| A4 | `transport` | ✅ | S | weryfikacja Herdr znika |
| A5 | `discover` | ✅ / 💻 | S+K | Herdr i klucze FIDO zostają w kliencie |
| A6 | wykrywanie profili sieci | ✅ | S | — |
| A7 | pojemność storage, `storage_layouts` | ✅ | S | — |
| A8 | osierocone wolumeny | ✅ | S | zależy od konwencji nazw (7.2) |
| B1 | `create` na nodzie Gold | 🟡 | S (+K) | snippety, zmiana nazwy roota |
| B2 | `create --node` (strumień) | ❌ | S | `--stdout` i `--archive -` tylko w CLI |
| B3 | `refresh` | 🟡 | S | snippety, dziennik, brak `disk import` |
| B4 | odzysk przerwanego `refresh` | 🟡 | S | dziennik na dysku noda |
| B5 | `resize` | 🟢 (💻 hasło LUKS) | S (+K) | hasło LUKS idzie dziś przez SSH roota |
| B6 | `migrate` | 🟡 | S | kopiowanie snippetów |
| B7 | `destroy` | 🟡 | S+K | usuwanie snippetów, sprzątanie lokalne |
| C1 | generowanie i zapis snippetów | ❌ | S | API nie przyjmuje typu `snippets` |
| C2 | wykrywanie starych snippetów | 🟡 | S | `test -e` i `rm` na nodzie |
| C3 | kopiowanie snippetów na inny node | ❌ | S | relay `ssh`/`scp` |
| C4 | `addkey` | 🟡 | S+K | edycja snippetów vendor/meta |
| C5 | klucze publiczne z Gold | ✅ / 🟢 | S | fallback przez QGA |
| D1 | czekanie na QGA | ✅ | S | — |
| D2 | weryfikacja gościa | 🟢 | S | część kontroli tylko przez `exec` |
| D3 | zajętość home | ✅ | S | `get-fsinfo` zamiast `df` |
| E1 | Gold: kontrakt po stronie PVE | ✅ | S | — |
| E2 | Gold: kontrakt gościa | 🟢 | S | wymaga `exec` |
| E3 | tagi ról i `homestack-lock` | ✅ | S | reguły uprawnień do tagów |
| F1 | instalator | ✅ / 💻 | K (+S) | kreator w kliencie, odczyty z serwera |
| F2 | wczytanie i walidacja configu | n/d | S+K | podział configu (8.1) |
| F3 | wgranie configu (nowe) | n/d | S | nowy endpoint (8.2) |
| G1 | `setup list` | 💻 | K | `gh` na desktopie |
| G2 | `setup status` | 💻 | K | SSH do workspace |
| G3 | Files i Credentials | 💻 | K | pliki źródłowe są na desktopie |
| G4 | Environment | 💻 / 🟢 | K | możliwe przez QGA (7.5) |
| G5 | Applications | 💻 / 🟢 | K | instalatory interaktywne wymagają TTY |
| G6 | BK backup | 💻 | K | klucze odbioru na desktopie |
| G7 | Herdr | 💻 | K | `herdr machine add` na desktopie |
| G8 | `setup --local` | 💻 | K | tylko desktop |
| H1–H3 | `repo` (inspect, setup, rotate) | 💻 | K | `gh` i SSH; serwer tylko z tokenem GitHub |
| I1 | lokalny SSH config, `known_hosts`, `identity_files` | 💻 | K | serwer zwraca dane, klient zapisuje |
| I2 | FIDO, sygnały dotyku, dźwięki | 💻 | K | — |

---

## 2. Najważniejsze ustalenia

1. **Prawie cała logika już używa API.** HomeStack rozmawia z Proxmoxem przez `pvesh`, `qm` i `pvesm`, które są cienką nakładką na to samo API. Zamiana transportu na HTTPS z tokenem nie zmienia algorytmów (plan → potwierdzenie → wykonanie → weryfikacja), tylko sposób wywołania i obsługę zadań (T4).
2. **Są cztery twarde luki.** Wszystkie wynikają z dostępu do systemu plików albo CLI noda:
   - zapis, odczyt, kopiowanie i usuwanie **snippetów Cloud-Init** (upload w API przyjmuje tylko `iso`, `vztmpl`, `import`),
   - **zmiana nazwy wolumenu** przez `perl PVE::Storage::rename_volume` (API potrafi alokować wolumen z nazwą albo przenieść go do innej VM z nazwą automatyczną, ale nie zmienić nazwy),
   - **strumień `vzdump --stdout | qm create --archive -`**, bo oba parametry są w API jawnie zablokowane poza CLI,
   - **dziennik `refresh`** zapisywany jako plik w katalogu snippetów na nodzie.
3. **Operacje w gościu przechodzą przez API, ale z dużym uprawnieniem.** `agent/exec` wymaga `VM.GuestAgent.Unrestricted`, czyli roota w każdym workspace z poola. Część odczytów (hostname, IP, montowania, zajętość dysku, numer seryjny dysku home) obsłużą endpointy `agent/get-*` z samym `VM.GuestAgent.Audit`.
4. **Setup, repo i backup to nie jest kwestia API Proxmoxa.** Działają przez SSH do workspace z kluczem sprzętowym na desktopie. Serwer w LXC tego klucza nie ma. To decyzja architektoniczna (7.5), a nie luka API.
5. **Część funkcji ma efekt tylko na desktopie:** `~/.ssh/config.d/homestack`, `known_hosts`, klucze odbioru BK, `identity_files` w configu, `herdr machine add`, `gh`. Zostają w kliencie, a serwer zwraca dane, których klient potrzebuje.
6. **Routing na node znika.** Dziś control node robi `ssh root@<node>` do pozostałych. W API każdy endpoint `/nodes/{node}/...` jest przekazywany przez dowolny node klastra (`proxyto`). Serwerowi wystarczy lista adresów API do failoveru.

---

## 3. Fundamenty wspólne dla wszystkich funkcji

### T1. Transport

- **Dziś:** `transports/herdr.py`. Jeden zalogowany shell roota w panelu Herdr, polecenia w kopercie begin/end, parsowanie wyjścia.
- **API:** HTTPS do `https://<node>:8006/api2/json/...` z nagłówkiem `Authorization: PVEAPIToken=USER@REALM!TOKENID=SECRET`.
- **TLS:** węzły mają już certyfikaty ACME (`pveX.efez.net`), więc klient API weryfikuje certyfikat normalnie, bez `verify=False`. W configu serwera trzeba używać nazw z certyfikatu, nie adresów IP.
- **Failover:** lista adresów node’ów w configu serwera, próbowana po kolei. Wszystkie odczyty i zapisy działają przez dowolny node z kworum.
- **Kontrakt:** obecny protokół `Transport` (`run`, `run_with_progress`, `run_json_value`, `execution_info`) jest oparty na poleceniach shella. Pod API trzeba nowego interfejsu z metodami `get/post/put/delete` i obsługą zadań. Logikę z `lifecycle.py` przepina się wtedy wywołanie po wywołaniu.

### T2. Routing na node

- **Dziś:** `proxmox.node_run` i `node_shell_command`: polecenie lokalnie na control node albo `ssh -o BatchMode=yes root@<node> …`.
- **API:** zbędne. Ścieżka `/nodes/{node}/...` wskazuje node docelowy, a Proxmox sam przekazuje zapytanie.

### T3. Sprawdzenie wymagań

- **Dziś:** `proxmox.check_remote_requirements` sprawdza na nodzie `qm`, `pvesh`, `pvesm`, `perl`, `base64`, `ssh`, `scp`, `timeout`. `create_transfer` dodatkowo `vzdump` i `bash`.
- **API:** zamienić na sondę przy starcie i przed planem: `GET /version`, `GET /access/permissions` (czy token ma wymagane uprawnienia na pool, storage i bridge), sprawdzenie typów treści storage (`images`, `snippets`).

### T4. Zadania i postęp

- **Dziś:** CLI wraca po zakończeniu, a postęp HomeStack parsuje regexami z wyjścia (`CLONE_TRANSFER_RE`, `MIGRATION_LV_CREATED_RE`, `MIGRATION_DD_PROGRESS_RE`).
- **API:** operacje długie (`clone`, `migrate`, `resize`, `status/start`, `status/shutdown`, `DELETE qemu`, `move_disk`, `POST config` z importem dysku) zwracają **UPID**.
  - Status: `GET /nodes/{node}/tasks/{upid}/status`.
  - Log: `GET /nodes/{node}/tasks/{upid}/log?start=N` z tymi samymi liniami, które dziś idą na stdout, więc regexy postępu zostają, zmienia się tylko źródło.
  - `PUT /config` jest synchroniczny, `POST /config` asynchroniczny (UPID). Do zmian dysków z importem używać `POST`.

### T5. Optymistyczna blokada

- **Dziś:** `--digest` w `refresh` (`_digest_args`) i `resize`.
- **API:** parametr `digest` w `PUT/POST /qemu/{vmid}/config`, `PUT /resize`, `PUT /unlink`. Ten sam mechanizm przyda się do koordynacji kilku instancji serwera (8.3).

### T6. Wykonanie polecenia w gościu

- **Dziś:** `guest._complete_guest_exec`: `qm guest exec VMID -- /bin/sh -lc CMD`, potem `qm guest exec-status VMID PID` w pętli. Ping: `qm guest cmd VMID ping`.
- **API:**
  - `POST /nodes/{n}/qemu/{vmid}/agent/exec` z `command=["/bin/sh","-lc",CMD]` (tablica, nie string) i opcjonalnym `input-data` do 64 KiB,
  - `GET /nodes/{n}/qemu/{vmid}/agent/exec-status?pid=PID`,
  - `POST /nodes/{n}/qemu/{vmid}/agent/ping`.
- **Uprawnienia:** `exec` i `exec-status` wymagają `VM.GuestAgent.Unrestricted`. `ping` i `get-*` wystarczy `VM.GuestAgent.Audit`.

### T7. Operacje plikowe na nodzie

- **Dziś:** `cloudinit.remote_write_text` (`printf … | base64 -d > plik`), `guest.remote_path_exists_on_node` (`test -e`), `rm -f`, `cat`, `mkdir -p`, `mktemp`.
- **API:** brak. Dotyczy C1–C4, B3, B4 i B7. Propozycje w 7.1 i 7.4.

---

## 4. Mapowanie per kategoria

Każda funkcja ma ten sam układ: **Kod**, **Dziś**, **API**, **Luki**, **Zależności**, **Uprawnienia**.

### A. Inwentarz i odczyty

#### A1. `status` globalny ✅ — S

- **Kod:** `status.global_status`.
- **Dziś:** `pvesh get /cluster/resources --type vm`, `pvesh get /nodes`, config każdej VM HomeStacka (Gold, `homestack-ws`, `homestack-alien`), pojemność storage (A7), skan osieroconych wolumenów (A8), zajętość home przez QGA (D3). Node’y offline są pomijane.
- **API:** te same ścieżki: `GET /cluster/resources?type=vm`, `GET /nodes`, `GET /nodes/{n}/qemu/{vmid}/config`, `GET /nodes/{n}/storage/{s}/status`.
- **Luki:** brak. Zajętość home przejdzie na `get-fsinfo` (D3), bez `exec`.
- **Zależności:** A7, A8, D3, config (`gold_vmid`, `storage_layouts`, `home.disk`, `network.prefix`, `user.name`).
- **Uprawnienia:** `VM.Audit`, `Datastore.Audit`, `Sys.Audit` na `/nodes`, `VM.GuestAgent.Audit`.

#### A2. `status VM` 🟢 — S

- **Kod:** `status.workspace_status`.
- **Dziś:** config i status VM, rola z tagów, IP z VMID (`network.prefix.<vmid>`). Dla działającej VM przez QGA `exec`: `hostname`, IP na `eth0`, `findmnt` dla `/home/<user>`, etykieta przez `blkid`, `getent passwd ubuntu`.
- **API:** config i status ✅. Część z gościa da się przenieść na endpointy tylko do odczytu:
  - `GET agent/get-host-name` → hostname,
  - `GET agent/network-get-interfaces` → IP,
  - `GET agent/get-fsinfo` → `mountpoint`, `type`, `used-bytes`, `total-bytes` i `disk[].serial`.
- **Luki:** `get-fsinfo` nie zwraca etykiety systemu plików. Numer seryjny dysku (`HS_HOME_<vmid>`, ustawiany przez HomeStack w `serial=`) jest równoważnym dowodem, że w `/home/<user>` jest właściwy dysk. Sprawdzenie użytkownika `ubuntu` wymaga `exec` albo można je przenieść do E2.
- **Propozycja:** status na poziomie `Audit`, a `exec` tylko w trybie „szczegółowym”.
- **Uprawnienia:** `VM.Audit`, `VM.GuestAgent.Audit` (+ `Unrestricted` dla trybu szczegółowego).

#### A3. Rozpoznanie celu (nazwa → VMID) ✅ — S

- **Kod:** `lifecycle.resolve_workspace_target`, `repo.repository_workspace_info`, `setup.resolve_target`.
- **Dziś:** `pvesh get /cluster/resources --type vm`, potem config każdej VM o pasującej nazwie i sprawdzenie tagów.
- **API:** `GET /cluster/resources?type=vm`, `GET /qemu/{vmid}/config`. `cluster/resources` zwraca też `tags` i `name`, więc często wystarczy jedno zapytanie.
- **Uwaga:** IP jest liczone z VMID (`guest.derive_ip`, VMID 2–254). To logika configu, nie API.
- **Zależności:** używane przez prawie wszystkie operacje i przez klienta przed setupem i repo (G, H).

#### A4. `transport` ✅ — S

- **Kod:** `status.transport_status`, `HerdrSession.verify`.
- **Dziś:** weryfikacja panelu Herdr (workspace, tab, jeden pane, SSH, prompt roota, UID 0, hostname), potem `pvesh get /nodes/{node}/status` (`pveversion`, `kversion`, `uptime`).
- **API:** `GET /version`, `GET /nodes/{node}/status`. Część Herdr znika.
- **Nowe:** zdrowie serwera (`/health/live` jak w mypowers) i sprawdzenie tokenu (`GET /access/permissions`).

#### A5. `discover` ✅ / 💻 — S+K

- **Kod:** `discovery.discover_environment`, `discover_hardware_identities`.
- **Dziś:** lokalne polecenia (`herdr`, `ssh`, `rsync`), kandydaci Herdr w workspace `PVE`, `pvesh get /version` przez każdego kandydata, lista stubów kluczy FIDO w `~/.ssh`.
- **API:** część PVE jako `GET /v1/discovery` na serwerze, tylko odczyt: wersje, node’y, storage z typami treści, bridge, DNS, VM z tagiem Gold.
- **Klient:** wykrywanie Herdr i kluczy FIDO zostaje w kliencie.

#### A6. Wykrywanie profili sieci ✅ — S

- **Kod:** `discovery.discover_workspace_networks`.
- **Dziś:** bridge z `net0` Gold, `ipconfig0` i `nameserver` workspace’ów, `pvesh get /nodes/{n}/dns`, `pvesh get /nodes/{n}/network`.
- **API:** `GET /qemu/{vmid}/config`, `GET /nodes/{n}/dns`, `GET /nodes/{n}/network`.

#### A7. Pojemność storage i `storage_layouts` ✅ — S

- **Kod:** `status.homestack_storage_capacities`, `proxmox.resolve_homestack_storage`, `homestack_storage_layout_name`.
- **Dziś:** `pvesh get /nodes/{n}/storage/{s}/status`. Nazwa layoutu pochodzi od końcowej liczby w nazwie noda (`pve2` → `homestack-storage-2`).
- **API:** `GET /nodes/{n}/storage/{s}/status`.

#### A8. Osierocone wolumeny ✅ — S

- **Kod:** `proxmox.orphaned_homestack_volumes`, `homestack_volume_identity`.
- **Dziś:** `pvesh get /nodes/{n}/storage` i `.../storage/{s}/content --content images`. Wolumen jest rozpoznawany po nazwie `vm-<vmid>-hs-(root|home)-<nazwa>` i porównywany z dyskami podpiętymi do VM.
- **API:** `GET /nodes/{n}/storage`, `GET /nodes/{n}/storage/{s}/content?content=images` (odpowiedź zawiera też `vmid` właściciela).
- **Luki:** zależy od konwencji nazw. Jeśli zrezygnujesz ze zmiany nazwy roota (7.2), rooty trzeba rozpoznawać inaczej, np. jako wolumen należący do VMID, którego nie ma w klastrze.

### B. Cykl życia VM

#### B1. `create` na nodzie Gold 🟡 — S (+K)

- **Kod:** `lifecycle.build_create_plan`, `create_workspace`, `clone_full`.
- **Kroki i mapowanie:**
  1. Stare snippety dla nazwy: wykrycie i usunięcie (C2). ❌
  2. Klucze publiczne z Gold (C5). ✅ / 🟢
  3. `qm clone GOLD NEW --name NAME --full 1 --storage S` → `POST /nodes/{gold_node}/qemu/{gold}/clone` z `newid`, `name`, `full=1`, `storage`. Zwraca UPID. Warto dodać `pool=homestack`, żeby token dostał `VM.Allocate` przez pool. ✅
  4. Zmiana nazwy roota na `vm-<id>-hs-root-default` (`rename_attached_disk_volume` → `perl PVE::Storage::rename_volume`, potem `qm set --scsi0`, potem usunięcie starego `unusedN`). ❌ (7.2)
  5. Alokacja home: `pvesm alloc S VMID vm-<id>-hs-home-<user> <N>G --format raw` → `POST /nodes/{n}/storage/{s}/content` z `vmid`, `filename`, `size`, `format=raw`. API przyjmuje własną nazwę. ✅
  6. Podpięcie home: `qm set --scsi1 VOL,discard=on,iothread=1,ssd=1,serial=HS_HOME_<id>` → `POST /qemu/{vmid}/config`. Przy błędzie `pvesm free VOL` → `DELETE /nodes/{n}/storage/{s}/content/{volid}`. ✅
  7. Tagi: `qm set --tags homestack-ws` (nadpisuje odziedziczony `homestack-gold`) → `tags` w config. ✅
  8. Snippety użytkownika (C1). ❌
  9. `qm set --ipconfig0 …`, `qm set --cicustom …`, `qm cloudinit update` → config + `PUT /qemu/{vmid}/cloudinit`. ✅
  10. `qm start` → `POST /qemu/{vmid}/status/start`. ✅
  11. Weryfikacja (D1, D2). 🟢
  12. Lokalnie: usunięcie hosta z `known_hosts`, zapis `~/.ssh/config.d/homestack/vm<id>-<name>.conf` (I1). 💻
- **Sprzątanie przy błędzie przed startem:** `qm destroy VMID --purge 1` → `DELETE /qemu/{vmid}?purge=1` ✅. Usunięcie zapisanych snippetów ❌.
- **Zależności:** C1, C2, C5, D1, D2, E3, I1. Konwencja nazw wpływa na A8 i B7.
- **Uprawnienia:** `VM.Clone` na Gold, `VM.Allocate` na nowym VMID lub poolu, `Datastore.AllocateSpace` na storage HomeStacka, `SDN.Use` na bridge z `net0`, `VM.Config.Disk`, `VM.Config.Cloudinit` (także `ipconfig0` i `sshkeys`), `VM.Config.Options` (tagi), `VM.PowerMgmt`, `Datastore.AllocateSpace` na storage snippetów (wymagane przy `cicustom`).

#### B2. `create --node` (strumień Gold na inny node) ❌ — S

- **Kod:** `create_transfer.validate_cross_node_create`, `build_stream_restore_command`, `stream_gold_restore`, `lifecycle._verify_streamed_restore`.
- **Dziś:** `bash -o pipefail -c "vzdump GOLD --stdout 1 --mode snapshot --compress 0 --fleecing enabled=0 … | ssh root@TARGET qm create NEW --archive - --storage S --unique 1 --start 0 --template 0 --name NAME"`. Bez pliku pośredniego, bez miejsca na kopię roota na nodzie Gold. Potem kroki 4–12 z B1.
- **API:** brak dla samego strumienia.
  - `vzdump` z `stdout`: API zwraca błąd, że opcja jest dozwolona tylko w CLI.
  - `POST /nodes/{n}/qemu` z `archive="-"`: kod zgłasza, że potok wymaga środowiska CLI.
- **Co działa przez API:** walidacja (statusy node’ów, treści `images` i `snippets` na docelowym storage, bridge) ✅ i weryfikacja odtworzonej VM (sloty dysków, storage, opcje roota, bridge, nazwa, flaga template, boot order) ✅.
- **Propozycje:** 7.3.
- **Uprawnienia:** jak B1, plus zależnie od wariantu `VM.Backup` na Gold, `VM.Migrate` i `Datastore.AllocateSpace` na storage kopii.

#### B3. `refresh` 🟡 — S

- **Kod:** `lifecycle.build_refresh_plan`, `refresh_workspace`, `_unlink_disk`, `_set_vm_values`, `_root_attachment_spec`, `_delete_unused_volume`.
- **Warunki:** VM musi być na nodzie Gold, bez tagu `homestack-lock` i nie może być `homestack-alien`. Tagi są sprawdzane w planie, przed wykonaniem i przed przełączeniem roota.
- **Kroki i mapowanie:**
  1. Dziennik transakcji (`_write_refresh_journal`) w `snippet_dir/homestack-vm<id>-refresh.json`. ❌ (7.4)
  2. `pvesm path GOLD_VOL` i `qm disk import VMID ŚCIEŻKA S --target-disk unusedN`. ❌ jako takie, bo w API nie ma endpointu `disk import`. Odpowiednik: `import-from` w specyfikacji dysku, `POST /qemu/{vmid}/config` z `scsiN="<S>:0,import-from=<volid roota Gold>"`. Źródło musi być wolumenem zarządzanym przez PVE (wtedy wymagane `VM.Clone` na Gold). Ścieżka bezwzględna działa tylko dla `root@pam`. `import-from` nie celuje bezpośrednio w `unusedN`, więc są dwa warianty (test na Twoim storage):
     - **Import przy działającej VM do wolnego slotu** (np. `scsi29`), potem `PUT /unlink` bez `force` → dysk staje się `unusedN`. Krótki przestój jak dziś, ale gość na chwilę widzi dodatkowy dysk (hotplug).
     - **Import po zatrzymaniu prosto do `scsi0`.** Stary root trafia do `unusedN`. Prostsze, ale dłuższy przestój i inna kolejność faz w dzienniku.
  3. `qm shutdown --timeout 90` → `POST /status/shutdown` z `timeout`. HomeStack nie robi twardego stopu. ✅
  4. Przełączenie roota: `qm disk unlink --idlist scsi0 --digest` → `PUT /qemu/{vmid}/unlink` z `idlist`, `digest`. Podpięcie nowego roota z opcjami z Gold (`iothread`, `discard`, `ssd`, `cache`, `aio`, `backup`, `replicate`, `ro`) → config. ✅
  5. Snippety z `preserve_home=True` (`allow_format=0`) (C1). ❌
  6. `boot`, `ipconfig0`, `cicustom` → config, `PUT /cloudinit`. ✅
  7. Start i weryfikacja (D1, D2). 🟢
  8. Przywrócenie stanu zasilania (zatrzymanie, jeśli VM była zatrzymana). ✅
  9. Usunięcie starego roota: `qm disk unlink --idlist unusedN --force 1` → `PUT /unlink` z `force=1`. ✅
  10. Usunięcie dziennika. ❌
  11. Lokalnie: `known_hosts` (I1). 💻
- **Zależności:** B4, C1, D1, D2, E3, I1.
- **Uprawnienia:** `VM.Clone` na Gold, `VM.Config.Disk` (także `boot`), `VM.Config.Cloudinit`, `VM.PowerMgmt`, `Datastore.AllocateSpace`, `VM.GuestAgent.Unrestricted`.

#### B4. Odzysk przerwanego `refresh` 🟡 — S

- **Kod:** `lifecycle._read_refresh_journal`, `_validate_refresh_journal`, `_recover_refresh`.
- **Dziś:** dziennik ma fazy `prepared → imported → switching → switched → verifying → verified`. Odzysk przywraca oryginalny root albo kończy sprzątanie po zweryfikowanej fazie, a potem stan zasilania. Uruchamia się automatycznie po błędzie albo przy następnym `refresh` (wtedy plan ma `mode="recover"`).
- **API:** wszystkie kroki PVE ✅ (`unlink`, config, `cloudinit`, `start`, `shutdown`).
- **Luki:** odczyt, zapis i usunięcie pliku dziennika na nodzie. ❌ (7.4)
- **Zależności:** B3. Dziennik musi być widoczny dla każdej instancji serwera (8.3).

#### B5. `resize` 🟢 (💻 hasło LUKS) — S (+K)

- **Kod:** `resize.build_resize_plan`, `resize_workspace`, `_execute_resize`, `resize_auth.crypt_resize_access`, `resize_guest.INSPECT`.
- **Dziś:**
  - Plan: config, warunki (VM działa, brak `lock`, nie template i nie Gold, dokładnie dwa dyski danych).
  - Inspekcja gościa: `python3 -c INSPECT` przez QGA (skrypt ok. 16 KB, ext4/Btrfs, partycja, LUKS, LVM).
  - `qm disk resize VMID DYSK ROZMIAR --digest`.
  - W gościu: czekanie na nowy rozmiar, `growpart`, `cryptsetup resize`, `pvresize` + `lvextend`, `resize2fs` albo `btrfs filesystem resize`.
- **API:** `PUT /qemu/{vmid}/resize` z `disk`, `size`, `digest` ✅. Wszystko w gościu przez `agent/exec` 🟢. Skrypt jako argument `python3 -c` mieści się w limicie zapytania.
- **Luki:**
  - LUKS z plikiem klucza działa przez QGA 🟢.
  - LUKS z hasłem: dziś `getpass` w terminalu i `cryptsetup --key-file -` przez SSH roota z kluczem sprzętowym, z porównaniem `boot_id` z QGA. README wprost zakłada, że hasło nie idzie przez QGA. Warianty: zostawić ten tryb w kliencie (K łączy się sam przez SSH roota), albo przyjąć hasło w żądaniu do serwera i podać je przez `input-data` w `agent/exec` (nie w argumentach). To zmiana polityki, decyzja w 10.
- **Zależności:** A3, D1.
- **Uprawnienia:** `VM.Config.Disk`, `Datastore.AllocateSpace`, `VM.GuestAgent.Unrestricted`.

#### B6. `migrate` 🟡 — S

- **Kod:** `lifecycle.build_migrate_plan`, `migrate_workspace`, `MigrationProgressTracker`.
- **Kroki i mapowanie:**
  1. Plan: brak referencji `unusedN` (config), dostępność noda (`pvesh get /nodes/{target}/status`), storage z `storage_layouts`. ✅ Dodatkowo `GET /qemu/{vmid}/migrate?target=…` zwraca listę lokalnych dysków i ograniczenia, przydatne do planu.
  2. Kopiowanie snippetów na node docelowy przed wyłączeniem (C3). ❌
  3. `qm shutdown`. ✅
  4. `qm migrate VMID TARGET --with-local-disks --targetstorage S` → `POST /nodes/{src}/qemu/{vmid}/migrate` z `target`, `with-local-disks=1`, `targetstorage`, offline. UPID, postęp z logu zadania (T4). ✅
  5. Weryfikacja: node w `cluster/resources`, `serial` home w config. ✅
  6. `qm cloudinit update` na nowym nodzie, start, weryfikacja montowania home przez QGA (lub `get-fsinfo`), stan zasilania. ✅ / 🟢
  7. Przy błędzie po wyłączeniu: automatyczny start na nodzie źródłowym. ✅
- **Luki:** tylko C3. Znika przy wspólnym storage snippetów albo statycznym snippecie (7.1).
- **Zależności:** C3, D1.
- **Uprawnienia:** `VM.Migrate`, `VM.PowerMgmt`, `VM.Config.Cloudinit`, `Datastore.AllocateSpace` na storage docelowym, `VM.GuestAgent.Audit`.

#### B7. `destroy` 🟡 — S+K

- **Kod:** `lifecycle.build_destroy_plan`, `destroy_workspace`.
- **Kroki i mapowanie:**
  1. Plan: tożsamość workspace, pojemność storage (`.../storage/{s}/status`), zajętość home (D3), ocena ryzyka. ✅
  2. Ponowna weryfikacja etykiety home, `qm shutdown`. ✅
  3. `qm destroy VMID --purge 1` → `DELETE /nodes/{n}/qemu/{vmid}?purge=1`. ✅
  4. Usunięcie snippetów i dziennika na **wszystkich** node’ach (`rm -f`). ❌
  5. Lokalnie: usunięcie kluczy odbioru BK (`backup.cleanup_retrieval_keys`), SSH configu VM, wpisu w `known_hosts`. 💻
- **Uwaga:** kolejność klient–serwer. Serwer kończy krok 3–4 i zwraca VMID, IP i nazwę, a klient robi krok 5. Jeśli klient nie zdąży, potrzebny jest osobny endpoint/komenda „sprzątnij lokalnie po VMID”.
- **Zależności:** C2 (te same nazwy plików), G6, I1.
- **Uprawnienia:** `VM.Allocate`, `VM.PowerMgmt`, `VM.Audit`.

### C. Cloud-Init i klucze SSH

#### C1. Generowanie i zapis snippetów ❌ — S

- **Kod:** `cloudinit.write_snippets`, `snippet_names`, `cicustom_value`, `remote_write_text`.
- **Dziś:** cztery pliki na VM w `cfg.snippet_dir` (domyślnie `/var/lib/vz/snippets`) na nodzie VM: `homestack-<name>-{user,vendor,network,meta}.yaml`.
  - `user`: hostname, `ssh_pwauth: false`, `ssh_deletekeys: true`, `disable_root: false`.
  - `vendor`: skrypt `runcmd` znajduje dokładnie jeden dysk o serialu `HS_HOME_<id>`, formatuje go tylko gdy pusty i `allow_format=1`, montuje przez etykietę w `/etc/fstab`, przy pierwszym formatowaniu zakłada `~/.ssh/authorized_keys`, włącza linger, jeśli w home jest usługa Herdr.
  - `network`: statyczny IP dopasowany po MAC, renderer NetworkManager, DNS.
  - `meta`: losowy `instance-id`.
- **API:** generowanie to czysta logika serwera. Zapis pliku na node ❌, bo upload przyjmuje tylko `iso`, `vztmpl`, `import`. Samo `cicustom` w config ✅ (z `Datastore.AllocateSpace` na storage snippetów).
- **Propozycje:** 7.1.
- **Zależności:** B1, B3, C4 (oczekuje konkretnej struktury vendor), C2, C3, B7.

#### C2. Wykrywanie i usuwanie starych snippetów 🟡 — S

- **Kod:** `cloudinit.stale_create_snippets`, `snippet_references`, `remove_stale_create_snippets`.
- **Dziś:** `test -e` na plikach dla nazwy, skan `cicustom` we wszystkich VM klastra, `rm -f`, jeśli żadna VM ich nie używa.
- **API:** skan referencji ✅ (`cluster/resources` + config). Lista snippetów jest dostępna przez `GET /nodes/{n}/storage/{s}/content?content=snippets`, a `DELETE /nodes/{n}/storage/{s}/content/{volid}` przyjmuje też snippety, ale oba wymagają `Datastore.Allocate` na tym storage (szerokie uprawnienie: pozwala zmieniać i usuwać cały storage). Czy usunięcie faktycznie kasuje plik snippetu, trzeba sprawdzić testem. Bez tego uprawnienia ❌.

#### C3. Kopiowanie snippetów na inny node ❌ — S

- **Kod:** `cloudinit.sync_snippets_to_node`.
- **Dziś:** relay przez control node (`ssh`/`scp` do źródła i celu), bez zaufania między nodami źródła i celu.
- **API:** brak. Znika przy wspólnym storage snippetów (7.1 B) albo statycznym snippecie na każdym nodzie (7.1 A).

#### C4. `addkey` 🟡 — S+K

- **Kod:** `addkey.build_addkey_plan`, `add_key`, `update_vendor`, `ensure_keys_item`, `new_meta`, `register_identity`, `sync_local_ssh`.
- **Kroki i mapowanie:**
  1. Odczyt snippetów vendor i meta (`cat`). ❌
  2. `qm set --sshkeys <plik tymczasowy>` → `POST /qemu/{vmid}/config` z `sshkeys` (URL-encoded), bez pliku. ✅
  3. Przepisanie vendor (`allow_format=0`, jeden krok `ensure-keys`, który tylko dopisuje brakujące klucze dla roota i użytkownika) i meta (nowy `instance-id`). ❌
  4. `qm cloudinit update`. ✅
  5. Restart tylko za zgodą (`--restart` albo pytanie, `--yes` nie restartuje): `shutdown`, `start`. ✅
  6. Czekanie na QGA, `cloud-init status --wait`, sprawdzenie kluczy w obu `authorized_keys`. 🟢
  7. Lokalnie: `known_hosts`, dopisanie klucza prywatnego do `identity_files` w configu, przepisanie SSH configu VM. 💻
- **Uwaga:** pytanie o restart przenosi się do klienta. Serwer zwraca stan `pending_restart`, a restart to osobne wywołanie.
- **Alternatywa dla działającej VM:** dopisanie klucza od razu przez `agent/exec` (albo `agent/file-write`), bez restartu. Wymaga `Unrestricted` (albo `VM.GuestAgent.FileWrite`).
- **Przy natywnym Cloud-Init (7.1 A):** zmiana `sshkeys` sama zmienia `instance-id` (Proxmox liczy go jako SHA1 z user-data i network-config), więc krok 3 znika.
- **Zależności:** C1 (struktura vendor), D1, I1.
- **Uprawnienia:** `VM.Config.Cloudinit`, `VM.PowerMgmt`, `VM.GuestAgent.Unrestricted`.

#### C5. Klucze publiczne workspace z Gold ✅ / 🟢 — S

- **Kod:** `workspace_ssh.get_workspace_authorized_keys`, `validate_authorized_keys`, `parse_authorized_key_records`.
- **Dziś:** `sshkeys` z configu Gold. Jeśli puste i Gold działa: QGA `cat` dla `/home/<user>/.ssh/authorized_keys`, potem `/root/.ssh/authorized_keys`.
- **API:** config ✅. Fallback przez `agent/exec` 🟢 albo `GET agent/file-read?file=…` (wystarczy `VM.GuestAgent.FileRead`).

### D. Weryfikacja gościa przez QGA

#### D1. Czekanie na QGA ✅ — S

- **Kod:** `guest.wait_for_qga_on_node`, `qga_ping_command`.
- **Dziś:** `timeout 5s qm guest cmd VMID ping` co 2 s do limitu.
- **API:** `POST agent/ping` w pętli. Wystarczy `VM.GuestAgent.Audit`.

#### D2. Weryfikacja gościa po `create`, `refresh` i `migrate` 🟢 — S

- **Kod:** blok weryfikacji w `create_workspace`, `_verify_refreshed_guest`, koniec `migrate_workspace`.
- **Kontrole i odpowiedniki:**

| Kontrola | Dziś | Najmniejsze uprawnienie w API |
|---|---|---|
| `cloud-init status --wait` | `exec` | `exec` (`Unrestricted`) |
| hostname = nazwa VM | `exec hostname` | `agent/get-host-name` (`Audit`) |
| IP na `eth0` | `exec ip -4 addr` | `agent/network-get-interfaces` (`Audit`) |
| `/home/<user>` to ext4 z właściwego dysku | `findmnt` + `blkid` | `agent/get-fsinfo`, serial `HS_HOME_<id>` (`Audit`) |
| UID/GID użytkownika | `exec id` | `agent/file-read /etc/passwd` (`FileRead`) |
| klucze w `authorized_keys` użytkownika i roota | `exec test`, `cat` | `agent/file-read` (`FileRead`) |
| tylko jeden zwykły użytkownik | `awk /etc/passwd` | `agent/file-read /etc/passwd` (`FileRead`) |
| brak `sudo` | `exec command -v sudo` | `exec` (`Unrestricted`) |
| `cloud-id` (informacyjnie) | `exec` | `exec` (`Unrestricted`) |

- **Wniosek:** bez `Unrestricted` zostają dwie kontrole: czekanie na Cloud-Init i brak `sudo`. Czekanie da się zastąpić pętlą `agent/file-read` na `/run/cloud-init/result.json`, a `sudo` sprawdzaniem plików binarnych przez `file-read` (do testu). Czyli weryfikację da się zrobić na `Audit` + `FileRead`, bez roota w gościu.

#### D3. Zajętość home ✅ — S

- **Kod:** `guest.workspace_home_usage`.
- **Dziś:** QGA ping, `findmnt | blkid` (etykieta), `df -B1`.
- **API:** `GET agent/get-fsinfo` zwraca `used-bytes`, `total-bytes`, `mountpoint` i serial dysku. Wystarczy `VM.GuestAgent.Audit`.

### E. Gold i tagi ról

#### E1. Gold: kontrakt po stronie PVE ✅ — S

- **Kod:** `gold.check_gold_readiness` (część `PVE`).
- **Kontrole:** tag `homestack-gold` bez tagów workspace, root w `scsi0`, brak dodatkowych dysków danych i NIC, poprawny `net0` z MAC, napęd Cloud-Init, włączona opcja `agent`, `scsi0` w boot order, stan zasilania, użyteczne `sshkeys`.
- **API:** `GET /qemu/{vmid}/config`, `GET /qemu/{vmid}/status/current`.

#### E2. Gold: kontrakt gościa 🟢 — S

- **Kod:** `gold.check_gold_readiness` (części `guest` i `security`).
- **Kontrole:** narzędzia (`cloud-init`, `sshd`, `nmcli`, `lsblk`, `blkid`, `wipefs`, `mkfs.ext4`, `mount`, `mountpoint`, `systemctl`, `systemd-escape`, `install`), opcjonalne `rsync`, `git`, `ssh-keygen`, konto użytkownika z UID/GID, polityka zwykłych użytkowników, brak `sudo`, klucze roota i użytkownika.
- **API:** `agent/exec` (`Unrestricted` na Gold). Część przez `file-read`.
- **Uwaga:** instalator nigdy nie uruchamia Gold do inspekcji. Serwer powinien zachować tę zasadę.

#### E3. Tagi ról i blokada `refresh` ✅ — S

- **Kod:** `proxmox.require_workspace_tag`, `require_setup_workspace_tag`, `require_gold_tag`, `require_refresh_unlocked`, `verify_workspace_role_tags`, `set_workspace_role_tags`.
- **Tagi:** `homestack-gold`, `homestack-ws`, `homestack-alien`, `homestack-lock`.
- **API:** odczyt z config albo `cluster/resources` ✅, zapis przez `tags` w config ✅.
- **Uwaga:** edycja tagów ma w Proxmoxie osobne reguły (`user-tag-access` i `registered-tags` w opcjach Datacenter). Jeśli tagi `homestack-*` są zarejestrowane, może je ustawiać tylko ktoś z `Sys.Modify`. Do sprawdzenia przy konfiguracji tokenu.

### F. Konfiguracja i instalator

#### F1. Instalator ✅ / 💻 — K (+S)

- **Kod:** `install.run_installer` i funkcje `_configure_*`, `_choose_gold`, `_check_gold_with_progress`.
- **Dziś:** interaktywny kreator z zapisem etapów do szkicu (`install draft`). Używa A5, A6, inwentarza klastra (`cluster_node_statuses`, `cluster_vm_resources`, `cluster_storage_definitions`), wyboru Gold i E1/E2, wykrycia storage snippetów (`_configure_snippets`: storage z treścią `snippets` i ścieżką), sieci i lokalnych kluczy FIDO.
- **Podział:** kreator zostaje w kliencie. Serwer daje endpointy tylko do odczytu (`/v1/discovery`, `/v1/gold/{vmid}/readiness`). Wynik to dwa dokumenty: config serwera i config klienta (8.1).

#### F2. Wczytanie i walidacja configu — S+K

- **Kod:** `config.load_config`, `setup_config.parse_setup`.
- **Podział:** ta sama walidacja po stronie serwera dla wgranego configu i po stronie klienta przed wysłaniem.

#### F3. Wgranie configu (nowe) — S

- W kodzie nie istnieje. Propozycja kontraktu w 8.2.

### G. Setup przestrzeni użytkownika (SSH do workspace)

Wspólny mechanizm dla G1–G7:

- **Kod:** `setup.py`, `setup_guest.py`, `setup_cli.py`, `setup_catalog.py`, `setup_config.py`, `setup_documents.py`, `setup_files.py`, `setup_herdr.py`, `backup.py`, `workspace_ssh.WorkspaceSSH`.
- **Dziś:** jeden SSH ControlMaster jako zwykły użytkownik, z kluczem sprzętowym z desktopu. Połączenia potomne nie mogą się uwierzytelnić samodzielnie. Program `setup_guest.py` (62,5 KB) idzie z JSON-em przez stdin do `python3 -`. Preflight przed zmianami, snapshoty w `~/.local/state/homestack/` w gościu, rejestr `setup.json` w persistent home.
- **Proxmox:** używany tylko do A3 (VMID, nazwa, node, tag, czy działa) i do wyliczenia IP. Reszta to nie jest API Proxmoxa.

| ID | Funkcja | Kod | Dlaczego 💻 |
|---|---|---|---|
| G1 | `setup list` | `setup_catalog.load_catalog`, `discover_repositories` | katalog z configu, repozytoria przez `gh api` na desktopie, snapshoty katalogu w `~/.local/state` desktopu |
| G2 | `setup status` | `setup.inspect_workspace_state` | odczyt przez SSH użytkownika |
| G3 | Files i Credentials | `setup.apply_entry` (`FileParams`), `WorkspaceSSH.transfer` | `rsync` plików z desktopu |
| G4 | Environment | `setup_guest.environment_updates` | generowanie profili powłoki w gościu przez SSH |
| G5 | Applications | `apply_entry` (`ApplicationParams`), `setup_documents.merge_document` | instalatory jako użytkownik, interaktywne w TTY, parsowanie TOML/JSON/YAML na desktopie, walidatory, kopie plików aplikacji z desktopu |
| G6 | BK backup | `backup.apply`, `reconcile_retrieval_keys` | dwa klucze ED25519 tworzone na desktopie (prywatne nie opuszczają desktopu), wpisy z wymuszonym `cat` w `authorized_keys` gościa |
| G7 | Herdr | `setup_herdr.apply`, `connect_desktop` | w gościu binarka, unit, `loginctl enable-linger`; na desktopie `herdr machine add/rename/enable` z dotknięciem klucza |
| G8 | `setup --local` | `setup_local.LocalSetup` | tylko lokalny użytkownik desktopu |

- **Wariant przez QGA (do rozważenia później):** `agent/exec` jako root + `runuser -u <user> --`. Ograniczenia z kodu Proxmoxa i HomeStacka:
  - brak TTY, więc instalatory interaktywne odpadają,
  - `input-data` do 64 KiB, a `setup_guest.py` ma 62,5 KB plus payload, więc program trzeba wgrać wcześniej przez `agent/file-write` (do 60 KiB na wywołanie),
  - `file-write` po 60 KiB zamiast `rsync`, więc katalogi z G3 są niepraktyczne,
  - root w gościu zamiast zwykłego użytkownika i bez bramki klucza sprzętowego.
- Szczegóły decyzji w 7.5.

### H. Repozytoria GitHub 💻 — K

- **Kod:** `repo.inspect_repository`, `setup_repository`, `rotate_repository_key`, `_gh`, `_keys`, `_add_key`, `_delete_key`.
- **H1 inspect:** stan checkoutu i kluczy przez SSH do workspace, lista deploy keys przez `gh api repos/{repo}/keys` na desktopie.
- **H2 setup:** `ssh-keygen` w gościu (klucz prywatny zostaje w workspace), odczyt klucza publicznego przez SSH, rejestracja `gh api --method POST repos/{repo}/keys` (read-write), `git clone` albo `fetch` + jedno `merge --ff-only`.
- **H3 rotate:** usunięcie starego deploy key, nowa para, rejestracja, weryfikacja.
- **Domyślne repo:** `[repo] owner` + nazwa workspace (`homestack repo dev` → `OWNER/dev`). Ta konwencja przyda się do planowanych domen (9).
- **Serwer:** możliwy tylko z poświadczeniem GitHub w serwerze (GitHub App z tokenami instalacyjnymi albo fine-grained PAT ograniczony do repo z `Administration: write`) i z dostępem do gościa (SSH albo QGA). Do decyzji razem z 7.5.

### I. Efekty lokalne na desktopie 💻 — K

- **I1:** `workspace_ssh.write_local_ssh_config`, `remove_local_ssh_config`, `forget_local_ssh_host`, `addkey.register_identity` (edytuje `identity_files` w `config.toml`). Serwer zwraca dane: VMID, nazwę, IP, użytkownika i flagę „klucz hosta zmieniony”, a klient zapisuje pliki.
- **I2:** `fido.py` (sandbox `bwrap`, `fido2-token`), `touch.py` (sygnały dźwiękowe przy dotyku). Tylko klient.

---

## 5. Uprawnienia tokenu

Proponowana rola `HomeStack` dla użytkownika `homestack@pve` z tokenem z **Privilege Separation**. Gold i workspace’y w poolu `homestack`.

| Ścieżka | Uprawnienia | Do czego |
|---|---|---|
| `/pool/homestack` | `VM.Audit` | A1–A3, E1, wszystkie plany |
| | `VM.Allocate` | B1, B2, B7 |
| | `VM.Clone` | B1, B3 (`import-from` z Gold) |
| | `VM.Config.Disk` | B1, B3, B5 (także `boot`) |
| | `VM.Config.Cloudinit` | B1, B3, B6, C4 (`cicustom`, `ipconfig0`, `sshkeys`) |
| | `VM.Config.Options` | tagi (E3), `description` (7.4) |
| | `VM.PowerMgmt` | start, shutdown |
| | `VM.Migrate` | B6 |
| | `VM.GuestAgent.Audit` | D1, D3, A2, część D2 |
| | `VM.GuestAgent.FileRead` | C5, część D2 |
| | `VM.GuestAgent.Unrestricted` | B5, E2, reszta D2 |
| | `VM.Backup` | tylko wariant 7.3 B |
| | `VM.Config.HWType` | tylko wariant 7.1 A ze `smbios1` |
| `/storage/<storage HomeStacka>` | `Datastore.AllocateSpace`, `Datastore.Audit` | alokacja, A7, A8 |
| `/storage/<storage snippetów>` | `Datastore.AllocateSpace` | wymagane przy ustawianiu `cicustom` |
| `/nodes` | `Sys.Audit` | node’y, sieć, DNS, statusy |
| `/sdn/zones/localnetwork/<bridge>` | `SDN.Use` | `clone` i odtworzenie VM z bridge |

Uwagi:

- `VM.GuestAgent.Unrestricted` to root w każdej VM poola. Jeśli D2 przejdzie na `Audit` + `FileRead`, zostaje potrzebne tylko do `resize` i kontroli Gold (E2).
- Gold w tym samym poolu oznacza, że token może go zmienić. Lepiej dać Gold w osobnym poolu z samym `VM.Audit` + `VM.Clone` (+ `Unrestricted`, jeśli E2 ma działać).

---

## 6. Zależności między funkcjami

Wszystko zależy od T1 (klient API z tokenem) i F2/F3 (config na serwerze).

| Funkcja | Wymaga | Co się psuje bez tego |
|---|---|---|
| B1 `create` | C5, C1, D1, D2 | bez C1 VM nie dostaje IP, hostname ani zamontowanego home; bez C5 nie ma kluczy |
| B2 `create --node` | wybór z 7.3, potem jak B1 | brak jakiegokolwiek tworzenia poza nodem Gold |
| B3 `refresh` | C1, B4, D1, D2, E3 | bez B4 brak bezpiecznego wycofania; bez D2 nie ma fazy `verified`, więc kod wycofuje zmianę zamiast skasować stary root |
| B4 odzysk | miejsce na dziennik (7.4) | przerwany `refresh` wymaga ręcznej naprawy |
| B5 `resize` | D1, `Unrestricted` | bez QGA brak inspekcji gościa, więc brak planu |
| B6 `migrate` | C3 (albo 7.1 A/B), D1 | bez snippetów na nodzie docelowym VM nie wystartuje (`cicustom` wskazuje brakujące pliki) |
| B7 `destroy` | C2 (nazwy plików), I1, G6 | zostają snippety na nodach, a w kliencie stare klucze BK i wpisy SSH |
| C4 `addkey` | C1 (struktura vendor z `allow_format` i `ensure-keys`), D1, I1 | przy 7.1 A zmienia się w samą zmianę `sshkeys` + restart |
| A1 `status` | A7, A8, D3 | A8 zależy od konwencji nazw (7.2) |
| G, H | A3 + dostęp do gościa (7.5) | serwer musi zwrócić VMID, IP, nazwę i stan, zanim klient połączy się z VM |
| G (grupa Repositories) | H2 | setup wywołuje `repo.setup_repository`, więc obie ścieżki muszą trafić w to samo miejsce (klient albo serwer) |

Skutki decyzji z sekcji 7:

- **7.1 A albo B:** C2 i C3 znikają, C4 się upraszcza, B6 i B7 przestają dotykać plików na nodach.
- **D2 na `Audit` + `FileRead`:** `Unrestricted` potrzebne tylko w B5 i E2.
- **7.5 A:** G i H zostają w kliencie; serwer potrzebuje tylko endpointu z danymi workspace.

---

## 7. Propozycje dla luk

### 7.1 Snippety Cloud-Init (C1–C4, B6, B7)

**A. Natywny Cloud-Init + jeden statyczny snippet vendor.** Do oceny jako docelowy.

- Proxmox sam generuje user-data, network-config i meta-data z configu VM: `name` → hostname, `ipconfig0`, `nameserver`, `sshkeys`, `ciuser`. `cicustom` może wskazać tylko `vendor=…`, a reszta zostaje natywna.
- Snippet vendor jest **jeden dla wszystkich VM**, generyczny, wgrany raz na każdy node przy instalacji (albo na wspólny storage). Znajduje dysk o serialu `HS_HOME_*`, etykieta = serial, UID/GID bierze z konta w Gold.
- Parametry per VM (np. flaga `allow_format` przy `refresh`) przez `smbios1` (pola `family` albo `sku`, opcja `base64`), widoczne w gościu w `/sys/class/dmi/id/`. Wymaga `VM.Config.HWType`.
- Zmiana `sshkeys` zmienia `instance-id` automatycznie.
- **Koszty:** zmiana kontraktu Gold. Natywny user-data robi inne rzeczy niż obecny: `package_upgrade: true` (trzeba ustawić `ciupgrade=0`), `users: - default` przy `ciuser` innym niż root (dla `ciuser=root` tego nie ma, za to klucze trafiają do roota i snippet vendor musi je skopiować do home przy pierwszym formatowaniu), brak `ssh_pwauth: false` (to musi zapewnić konfiguracja `sshd` w Gold). Do testu na Gold.

**B. Wspólny storage snippetów montowany w kontenerze serwera.**

- Storage NFS albo CephFS z treścią `snippets`, dostępny na wszystkich nodach. Serwer montuje go do zapisu i pisze pliki bezpośrednio, a `cicustom` wskazuje ten storage.
- Znikają C3 (kopiowanie przy `migrate`) i sprzątanie na każdym nodzie w B7. Tam może też leżeć dziennik `refresh` (7.4).
- **Koszty:** eksport NFS, montowanie w LXC (bind mount z hosta albo kontener uprzywilejowany) lub w Dockerze, ograniczenie dostępu do udziału, a VM zależą od dostępności tego storage przy starcie.

**C. Wąski helper przez SSH.**

- Serwer ma własny klucz SSH do każdego noda, zapisany w `/root/.ssh/authorized_keys` z `restrict,command="homestack-node-helper"`. Helper przyjmuje tylko: zapis, odczyt, usunięcie i sprawdzenie plików `homestack-*.yaml` i dziennika w katalogu snippetów, ewentualnie `rename-volume` (7.2) i `stream-gold` (7.3).
- **Koszty:** klucz programowy w zawsze włączonym kontenerze, co odchodzi od zasady „bez kluczy zapasowych”. Ryzyko ograniczone przez wymuszone polecenie i walidację nazw.

**D. Własny agent HTTP na każdym nodzie.** Odradzam. Wcześniejsza propozycja LXC przyjęła, że na hostach PVE nie stawiamy własnych demonów.

**Rekomendacja:** A, jeśli testy na Gold przejdą, a w przeciwnym razie B. C tylko jako przejściowy most.

### 7.2 Zmiana nazwy roota (B1, A8)

- **A. Bez zmiany nazwy roota.** Root zostaje z nazwą Proxmoxa (`vm-<id>-disk-N`). `refresh` już dziś akceptuje nazwę nadaną przez Proxmoxa. Home zachowuje `vm-<id>-hs-home-<user>`, bo alokacja przez API przyjmuje nazwę. W A8 rooty rozpoznaje się jako wolumeny VMID-ów, których nie ma w klastrze (pole `vmid` w liście treści storage).
- **B. Helper C z 7.1** z poleceniem `rename-volume`.
- **Rekomendacja:** A.

### 7.3 `create` na innym nodzie (B2)

- **A. Klon na nodzie Gold + migracja offline:** `POST clone`, potem `POST migrate` z `with-local-disks` i `targetstorage`. Tylko API. Koszt: chwilowe miejsce na root na nodzie Gold i dwie kopie danych.
- **B. Kopia Gold na storage kopii (najlepiej PBS) i odtworzenie na nodzie docelowym:** jeden `POST /nodes/{gold}/vzdump` (`vmid`, `storage`, `mode=snapshot`) na wersję Gold, potem `POST /nodes/{target}/qemu` z `archive=<volid kopii>`, `storage`, `unique=1`, `start=0`, `name`. Jedna kopia służy do wielu tworzeń. Wymaga `VM.Backup` na Gold i storage kopii.
- **C. Replika Gold na każdym nodzie:** szablon Gold kopiowany na każdy node po zmianie Gold, potem zawsze lokalny pełny klon. Najszybsze tworzenie, kosztem miejsca i synchronizacji wersji Gold.
- **D. Helper C z 7.1** ze `stream-gold`. Zachowuje obecną zaletę: zero miejsca pośredniego.
- **Rekomendacja:** B, jeśli masz PBS albo storage kopii, a w przeciwnym razie A.

### 7.4 Dziennik `refresh` (B3, B4)

- **A. Na wspólnym storage snippetów** (7.1 B).
- **B. W polu `description` VM:** blok w notatkach VM, np. `<!-- homestack-journal: <base64 JSON> -->`. Limit 8 KiB (dziennik ma ok. 1 KB). Replikowane w klastrze, zapis warunkowy przez `digest`, widoczne dla każdej instancji serwera. Może też pełnić rolę blokady operacji (8.3). Minus: widać to w notatkach w GUI.
- **C. Helper C z 7.1.**
- **Rekomendacja:** B, bo wystarcza samo API i działa przy wielu instancjach.

### 7.5 SSH do workspace a serwer (G, H, B5 z hasłem LUKS)

- **A. Podział.** Serwer robi operacje na poziomie Proxmoxa (A–F). Klient zostawia setup, repo i hasło LUKS na SSH z kluczem sprzętowym, a od serwera pobiera tylko dane workspace (`GET /v1/workspaces/{id}`: VMID, nazwa, node, IP, użytkownik, stan, `alien`). Zachowuje bramkę klucza sprzętowego i jednokierunkową barierę desktop → VM. **Rekomendacja na pierwszy etap.**
- **B. Serwer przez QGA.** Możliwe później dla części nieinteraktywnej (Environment, nieinteraktywne Applications, structured config, instalacja BK). Ograniczenia w sekcji G.
- **C. Serwer z własnym kluczem SSH do workspace’ów.** Technicznie najprostsze, ale to klucz programowy z dostępem do wszystkich workspace’ów w zawsze działającym kontenerze, sprzeczny z obecnym projektem.

**Nowa granica zaufania.** API serwera z tokenem Proxmoxa to w praktyce klucz do całego poola. Kto ma token klienta do serwera, może tworzyć i kasować VM. Pasowałoby to do Twojego podejścia z kluczami sprzętowymi: operacje niszczące (`destroy`, `refresh`, `migrate`) mogłyby wymagać podpisu żądania kluczem sprzętowym (np. `ssh-keygen -Y sign` kluczem `sk-` z dotknięciem YubiKey), a nie tylko tokenu Bearer.

---

## 8. Uwagi dla serwisu bezstanowego

### 8.1 Podział configu

Obecny `config.toml` miesza dane serwera i desktopu.

| Sekcja | Gdzie | Uwagi |
|---|---|---|
| `[node]` (`name`, `gold_vmid`), `[storage_layouts]`, `[root]`, `[home]`, `[network]`, `[cloud_init]`, `[user]` | serwer | rdzeń logiki PVE |
| `[control]` | — | znika (T2) |
| `[transport]`, `[transport.herdr]` | — | zastępuje lista adresów API |
| `[repo] owner` | serwer i klient | domyślne repo i (w przyszłości) domena |
| `[repo] checkout_root`, `sort` | klient (serwer, jeśli 7.5 B) | — |
| `[workspace_ssh]`, `[sounds]`, `[display]` | klient | klucze sprzętowe, dźwięki, formatowanie |
| `[[setup.groups]]`, `[[setup.items]]` | klient (serwer, jeśli 7.5 B) | ścieżki źródłowe są na desktopie |

**Sekrety poza configiem:** token PVE, a w przyszłości token GitHub, klucz Pangolina, klucze OVH DNS, dostęp do Vaultwardena. Nie wysyłać ich w treści wgrywanego configu. Osobny endpoint tylko do zapisu (`PUT /v1/secrets/{name}`, bez odczytu) albo pliki z uprawnieniami `0600` montowane do kontenera, jak `MYPOWERS_API_TOKEN_FILE` w mypowers.

### 8.2 Wgrywanie configu

- `PUT /v1/config` z `If-Match: <wersja>`. Serwer waliduje tym samym kodem co `load_config`, zapisuje atomowo i odsyła `config_version` (SHA-256 treści).
- `GET /v1/config/version` pozwala klientowi sprawdzić, czy wszystkie instancje mają tę samą wersję.
- Każde żądanie zmieniające stan niesie nagłówek z oczekiwaną wersją configu. Instancja z inną wersją odrzuca je (`409`), więc żadna replika nie działa na nieaktualnym configu.

### 8.3 Kilka instancji

- Jedynym wspólnym stanem jest Proxmox. Dwie instancje mogą jednocześnie zmieniać tę samą VM, więc potrzebne są: `digest` przy każdym zapisie (T5) i blokada operacji w samym Proxmoxie (dziennik w `description` z 7.4 B albo tymczasowy tag, np. `homestack-busy`).
- Klient wysyła operację zmieniającą stan do pierwszej osiągalnej instancji i odpytuje o postęp tę samą instancję.
- Klucze idempotencji w pamięci (jak w mypowers) nie przetrwają przełączenia na inną instancję. Po przełączeniu klient musi odczytać stan i zrobić nowy plan, nigdy nie powtarzać starego żądania na ślepo (ta sama zasada co w mypowers).

### 8.4 Plan, wykonanie i zadania

- CLI już rozdziela plan i wykonanie (`--yes`, `--json` z `confirmation_required`, kod wyjścia 3). W API: `POST /v1/workspaces/{id}/refresh/plan` zwraca plan z ID i skrótem, a `POST /v1/plans/{id}/apply` go wykonuje.
- Kod już sprawdza plan ponownie przed wykonaniem (`_revalidate_stream_source`, ponowny plan w `resize_workspace`, ponowne `resolve_existing_workspace` w `destroy` i `refresh`). To przenosi się wprost.
- Długie operacje jak w mypowers: `202` + `GET /v1/jobs/{id}`, zdarzenia przez WebSocket (`snapshot`, postęp, heartbeat). Zadanie HomeStacka obejmuje kilka UPID-ów Proxmoxa.

### 8.5 Rozszerzalność

- Każda operacja jako moduł z deklaracją: wymagane uprawnienia PVE, sekcja configu, endpointy, zdarzenia, które emituje (`workspace.created`, `workspace.destroyed`, `workspace.migrated`).
- Przy starcie sonda (T3) wyłącza moduły, którym brakuje uprawnień, i zgłasza to w `GET /v1/capabilities` (mypowers ma podobny `GET /api/v1/capabilities` dla funkcji stacji).
- Nowe integracje (sekcja 9) podpinają się do zdarzeń, a nie do kodu `lifecycle.py`.

---

## 9. Punkty podpięcia planowanych funkcji

Te funkcje nie mają odpowiednika w Proxmoxie i nie muszą go mieć. Ważne jest, w których miejscach obecnego przepływu się podpinają.

- **Domena dla VM** (VM `X`, repo `X`, domena `X`): po B1 zdarzenie `workspace.created`. Nazwa jest już wspólna dla VM, hostname i domyślnego repo (H). IP jest wyliczane z VMID, więc cel zasobu w Pangolinie (`network.prefix.<vmid>:port`) da się podać od razu. `migrate` nie zmienia IP. `destroy` usuwa zasób (zdarzenie `workspace.destroyed`).
- **Pangolin:** Integration API (zasób, cele na wszystkich Newtach zgodnie z planem sieci, tryb otwarty albo z logowaniem).
- **Dostęp do Vaultwardena:** wygenerowane hasło lub klucz zapisany przez `bw` (albo `bw serve`) na osobnym koncie z jedną kolekcją, zgodnie z planem sieci.
- **DNS i certyfikaty:** ACME w Proxmoxie dotyczy tylko `pveproxy`. Certyfikaty usług zostają przy Caddy albo Pangolinie. Jeśli serwer ma zarządzać DNS, to jako dostawca OVH API (przy wildcardzie na Pangolina zwykle niepotrzebne).

---

## 10. Otwarte decyzje

1. Snippety: 7.1 A (natywny Cloud-Init + statyczny vendor), B (wspólny storage) czy C (helper SSH)?
2. Czy rezygnujesz ze zmiany nazwy roota (7.2 A)?
3. `create` na innym nodzie: masz PBS albo storage kopii (7.3 B), czy klon + migracja (7.3 A)?
4. Dziennik `refresh` w `description` VM (7.4 B)?
5. Czy weryfikacja gościa ma działać bez `VM.GuestAgent.Unrestricted` (D2 na `Audit` + `FileRead`)?
6. Hasło LUKS przy `resize`: zostaje w kliencie czy może iść przez serwer do `input-data`?
7. Setup i repo: zostają w kliencie (7.5 A) w pierwszym etapie?
8. Uwierzytelnianie klientów do serwera: tylko token Bearer czy dodatkowo podpis kluczem sprzętowym dla operacji niszczących?
9. Gold w osobnym poolu z węższymi uprawnieniami?

---

## Źródła

HomeStack (`c09636b`):

- [README](https://github.com/hipotures/homestack/blob/c09636b/README.md)
- [cli.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/cli.py), [lifecycle.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/lifecycle.py), [proxmox.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/proxmox.py), [create_transfer.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/create_transfer.py), [cloudinit.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/cloudinit.py), [guest.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/guest.py), [status.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/status.py)
- [resize.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/resize.py), [resize_auth.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/resize_auth.py), [addkey.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/addkey.py), [gold.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/gold.py), [discovery.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/discovery.py), [workspace_ssh.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/workspace_ssh.py)
- [setup.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/setup.py), [repo.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/repo.py), [backup.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/backup.py), [setup_herdr.py](https://github.com/hipotures/homestack/blob/c09636b/src/homestack/setup_herdr.py), [config.example.toml](https://github.com/hipotures/homestack/blob/c09636b/config.example.toml)

mypowers (`296d7f7`):

- [docs/api.md](https://github.com/hipotures/mypowers/blob/296d7f7/docs/api.md), [docs/operations.md](https://github.com/hipotures/mypowers/blob/296d7f7/docs/operations.md), [docs/configuration.md](https://github.com/hipotures/mypowers/blob/296d7f7/docs/configuration.md)

Proxmox (mirrory na GitHubie):

- [qemu-server `API2/Qemu.pm`](https://github.com/proxmox/qemu-server/blob/a4efa5f/src/PVE/API2/Qemu.pm): `clone`, `migrate`, `resize`, `unlink`, `cloudinit`, `import-from`, `archive` z potokiem tylko w CLI, uprawnienia do `cicustom`
- [qemu-server `API2/Qemu/Agent.pm`](https://github.com/proxmox/qemu-server/blob/a4efa5f/src/PVE/API2/Qemu/Agent.pm): `exec` (`Unrestricted`), `file-read` (`FileRead`), `file-write` (`FileWrite`, 60 KiB), `get-*` (`Audit`)
- [qemu-server `QemuServer/Cloudinit.pm`](https://github.com/proxmox/qemu-server/blob/a4efa5f/src/PVE/QemuServer/Cloudinit.pm): natywny user-data, `instance-id` jako SHA1
- [pve-manager `API2/VZDump.pm`](https://github.com/proxmox/pve-manager/blob/76177f3/PVE/API2/VZDump.pm): `stdout` tylko w CLI
- [pve-storage `API2/Storage/Status.pm`](https://github.com/proxmox/pve-storage/blob/75b36a4/src/PVE/API2/Storage/Status.pm): upload tylko `iso`, `vztmpl`, `import`
- [pve-storage `API2/Storage/Content.pm`](https://github.com/proxmox/pve-storage/blob/75b36a4/src/PVE/API2/Storage/Content.pm), [pve-storage `Storage.pm`](https://github.com/proxmox/pve-storage/blob/75b36a4/src/PVE/Storage.pm) (`check_volume_access`: ścieżki tylko dla `root@pam`, snippety wymagają `Datastore.AllocateSpace`)
- [Proxmox VE API viewer](https://pve.proxmox.com/pve-docs/api-viewer/)
