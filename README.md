# frr-bootc

Ein [bootc](https://containers.github.io/bootc/)-Image, das [FRR](https://frrouting.org/)
als Router-Appliance betreibt und dafür gedacht ist, als VM unter
**OpenShift Virtualization (KubeVirt)** zu laufen.

FRR- und Netzwerk-Konfiguration werden **nicht** ins Image gebacken, sondern
zur Laufzeit aus zwei Kubernetes-`ConfigMaps` in die VM gemountet und dort
von systemd-Services überwacht und automatisch angewendet.

## Architektur

```
                         OpenShift / KubeVirt
                         ─────────────────────
 ConfigMap "frr-config"      ConfigMap "network-config"
        │                            │
        │ virtiofs                  │ virtiofs
        ▼                            ▼
 /run/config/frr (ro)        /run/config/network (ro)
        │                            │
        ▼                            ▼
 frr-config-sync.path        network-config-sync.path
 (inotify auf Verzeichnis)   (inotify auf Verzeichnis)
        │                            │
        ▼                            ▼
 frr-config-sync.service     frr-bootc-ifnaming.service (MAC → Name)
   → /etc/frr/*               network-config-sync.service
   → vtysh -C (Validierung)     → *.nmconnection nach NetworkManager
   → frr-reload.py (live)       → nmstatectl apply *.yml/*.yaml
   → bei daemons-Änderung:
     systemctl restart frr
```

Beide Config-Quellen werden per **virtiofs** (nicht als Disk-Image) in die
VM gemountet. Das ist der von KubeVirt vorgesehene Weg, um `ConfigMap`-,
`Secret`- und `ServiceAccount`-Inhalte 1:1 als Dateien in eine VM zu geben,
ohne ein Cloud-Init-ISO oder einen Reboot zu benötigen.

### Warum zwei getrennte Sync-Pfade?

- **FRR-Konfiguration** (`frr.conf`, `daemons`, `vtysh.conf`) wird bei jeder
  Änderung neu synchronisiert. `frr.conf`-Änderungen werden per
  `vtysh -C` validiert und dann live über `frr-reload.py` übernommen (kein
  Neustart, keine Unterbrechung laufender Sessions/Nachbarschaften, soweit
  FRR das zulässt). Ändert sich `daemons` (z. B. `bgpd` wird neu aktiviert),
  ist ein Neustart von `frr.service` unvermeidlich, da das die laufenden
  Daemon-Prozesse bestimmt.
- **Netzwerkkonfiguration** wird in zwei Schritten angewendet:
  1. `frr-bootc-ifnaming.service` läuft **vor** `NetworkManager.service`
     und schreibt aus `interfaces.yaml` `.link`-Dateien
     (`/etc/systemd/network/70-frr-bootc-<name>.link`), die jedes Interface
     anhand seiner MAC-Adresse fest auf einen Namen pinnen.
  2. `network-config-sync.service` läuft **nach** `NetworkManager.service`
     und wendet die eigentliche Konfiguration an (nmstate oder
     NetworkManager-Keyfiles).

  Diese Trennung ist nötig, weil Interface-Umbenennung vor dem Start von
  NetworkManager passieren muss, `nmstatectl` aber einen laufenden
  NetworkManager voraussetzt.

## Konfigurationsformat

### `frr-config` ConfigMap → `/run/config/frr`

Beliebige Dateien werden 1:1 nach `/etc/frr/` gespiegelt. Relevant sind
insbesondere:

- `daemons` — welche FRR-Daemons laufen (siehe FRR-Doku)
- `frr.conf` — die eigentliche Routing-Konfiguration
- `vtysh.conf` — optional

Siehe [`manifests/10-configmap-frr-config.yaml`](manifests/10-configmap-frr-config.yaml).

### `network-config` ConfigMap → `/run/config/network`

- `interfaces.yaml` (optional, aber empfohlen) — MAC-Adresse-zu-Name-Mapping:

  ```yaml
  interfaces:
    - mac: "02:00:00:12:34:01"
      name: eth-uplink1
  ```

- `*.yml` / `*.yaml` (außer `interfaces.yaml`) — [nmstate](https://nmstate.io/)
  Desired-State-Dokumente, werden per `nmstatectl apply` angewendet.
- `*.nmconnection` — rohe NetworkManager-Keyfiles, werden nach
  `/etc/NetworkManager/system-connections/` installiert und aktiviert.

Beide Formate (nmstate und NetworkManager-Keyfiles) können gleichzeitig
verwendet werden — es kommt nur darauf an, welche Dateiendung die jeweiligen
Keys in der ConfigMap haben.

Siehe [`manifests/11-configmap-network-config.yaml`](manifests/11-configmap-network-config.yaml).

## Netzwerk-Interfaces in OpenShift zielsicher hinzufügen

Das Kernproblem beim Hinzufügen zusätzlicher NICs zu einer VM ist, dass die
Reihenfolge, in der der Gast sie sieht (und damit der vom Kernel vergebene
Name wie `enp2s0`), nicht garantiert stabil ist — besonders wenn später
weitere Interfaces dazukommen oder die VM neu gestartet wird. Deshalb pinnt
dieses Image Interface-Namen an MAC-Adressen (siehe oben), und der Workflow
zum Hinzufügen einer neuen NIC ist bewusst zweigleisig, damit beide Seiten
(VM-Spec und Guest-Konfiguration) exakt zusammenpassen:

1. **MAC-Adresse festlegen.** Wählt eine feste, eindeutige MAC-Adresse für
   das neue Interface (z. B. aus dem lokal verwalteten Bereich `02:xx:xx:xx:xx:xx`).
2. **VM-Spec erweitern:** In der `VirtualMachine` unter
   `spec.template.spec.domain.devices.interfaces` einen neuen Eintrag mit
   `name` und genau dieser `macAddress` hinzufügen, dazu unter
   `spec.template.spec.networks` das passende `multus.networkName`
   (siehe [`manifests/20-networkattachmentdefinition.yaml`](manifests/20-networkattachmentdefinition.yaml)
   für ein Beispiel eines Bridge-`NetworkAttachmentDefinition`).
3. **`network-config` ConfigMap erweitern:** In `interfaces.yaml` einen
   Eintrag mit derselben MAC-Adresse und dem gewünschten Namen ergänzen,
   und in `nmstate.yml` (oder einer `*.nmconnection`-Datei) die
   Konfiguration für genau diesen Namen hinterlegen.
4. **VM neu starten.** KubeVirt hängt neue Bridge-Interfaces nicht ohne
   Neustart der VM an; erst danach greift außerdem
   `frr-bootc-ifnaming.service`, das vor NetworkManager läuft und das neue
   Interface umbenennt, bevor es von NetworkManager beansprucht wird.

Damit ist das Hinzufügen eines Interfaces "zielsicher": Der Name im Gast
hängt ausschließlich von der MAC-Adresse ab, die im VM-Spec explizit gesetzt
wurde — nicht von der PCI-Slot-Reihenfolge, in der KubeVirt Interfaces
anhängt.

> Änderungen an bereits vorhandenen Interfaces (IP-Adressen, Routing) über
> `nmstate.yml`/`*.nmconnection` werden dagegen **ohne Neustart** über
> `network-config-sync.path` live übernommen.

## Build

Voraussetzung: `podman` mit Zugriff auf ein privilegiertes
`bootc-image-builder`-Setup.

```console
$ ./build.sh [tag]
```

Das Skript:

1. baut das bootc-Image aus `Containerfile`,
2. wandelt es mit `bootc-image-builder` in ein `qcow2` um,
3. verpackt das `qcow2` als minimales `containerDisk`-Image
   (`containerdisk/Containerfile`), dem Format, das KubeVirt für
   `spec...volumes[].containerDisk.image` erwartet.

Anschließend das `containerDisk`-Image in eine für den Cluster erreichbare
Registry pushen und in der `VirtualMachine` referenzieren.

## Deploy

Voraussetzung: OpenShift Virtualization mit aktiviertem virtiofs-Feature-Gate
für beliebige Volumes (ConfigMap/Secret/ServiceAccount als Filesystem) —
siehe Kommentar in [`manifests/30-virtualmachine.yaml`](manifests/30-virtualmachine.yaml).

```console
$ oc apply -f manifests/00-namespace.yaml
$ oc apply -f manifests/10-configmap-frr-config.yaml
$ oc apply -f manifests/11-configmap-network-config.yaml
$ oc apply -f manifests/20-networkattachmentdefinition.yaml   # falls zusätzliche NICs benötigt werden
$ oc apply -f manifests/30-virtualmachine.yaml                # <registry>/... vorher anpassen
```

Konfigurationsänderungen danach einfach per `oc edit configmap/frr-config`
bzw. `oc edit configmap/network-config -n frr-bootc` vornehmen — die
Sync-Services in der VM übernehmen den Rest.

## Fehlersuche

- `oc logs`/Konsolenzugriff auf die VM, dann in der VM:
  `journalctl -u frr-config-sync.service -u network-config-sync.service -u frr-bootc-ifnaming.service`
- Aktuell angewendeter Konfigurations-Hash: `/var/lib/frr-bootc/*.sha256`
- FRR-Validierungsfehler landen zusätzlich in `/tmp/frr-config-check.log`
  innerhalb der VM.
- `nmstatectl show` bzw. `nmcli connection show` zur Prüfung des aktuellen
  Netzwerkzustands.
