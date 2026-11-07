//@ pragma UseQApplication
//@ pragma Env QS_NO_RELOAD_POPUP=1
//@ pragma Env QT_QUICK_CONTROLS_STYLE=Basic
//@ pragma Env QT_QUICK_FLICKABLE_WHEEL_DECELERATION=10000

// OsmiumOS installer. Lives inside the ii config so it is built from ii's own widgets
// (same look as ii's settings app). Run: qs -p ~/.config/quickshell/ii/osmium-installer.qml
// The work is done by `osmium-install` (Rust): disks, check, run (JSON lines on stdout).

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtQuick.Window
import Quickshell
import Quickshell.Io
import qs.services
import qs.modules.common
import qs.modules.common.widgets

ApplicationWindow {
    id: root
    visible: true
    title: "OsmiumOS Kurulum"
    onClosing: Qt.quit()
    minimumWidth: 800
    minimumHeight: 560
    width: 1100
    height: 750
    color: Appearance.m3colors.m3background
    property real contentPadding: 8

    // ---------- install config ----------
    property var disks: []
    property string disk: ""
    property bool encrypt: false
    property string passphrase: ""
    property string passphrase2: ""
    property string timezone: "Europe/Istanbul"
    property var timezones: ["Europe/Istanbul"]
    property string locale: "tr_TR.UTF-8"
    property string keymap: "trq"
    property string xkbLayout: "tr"
    property string fullname: ""
    property string username: ""
    property string hostname: "osmium"
    property string password: ""
    property string password2: ""
    property var builtinDots: []
    property string dots: "ii"
    property string dotsMode: "config"
    property var searchResults: []
    property string searchStatus: ""

    // ---------- progress ----------
    property bool installing: false
    property bool finished: false
    property real progress: 0
    property string progressText: ""
    property string errorText: ""
    property var log: []
    property int seconds: 0

    property var pages: [
        { name: "Hoş geldin", icon: "waving_hand" },
        { name: "Disk", icon: "hard_drive" },
        { name: "Bölge", icon: "language" },
        { name: "Kullanıcı", icon: "person" },
        { name: "Masaüstü", icon: "palette" },
        { name: "Kur", icon: "download" },
    ]
    property int currentPage: 0

    function clean(s) { return String(s).replace(/[\r\n]/g, "") }

    function configText() {
        const kv = {
            disk: disk, encrypt: encrypt ? "1" : "0", passphrase: encrypt ? passphrase : "",
            hostname: hostname, username: username, fullname: fullname, password: password,
            timezone: timezone, locale: locale, keymap: keymap, xkb_layout: xkbLayout,
            dots: dots, dots_mode: dotsMode,
        }
        return Object.keys(kv).map(k => `${k}=${clean(kv[k])}`).join("\n") + "\n"
    }

    // First problem that blocks installing, or "".
    function problem() {
        if (disk === "") return "Bir disk seç."
        if (encrypt && passphrase.length < 8) return "Şifreleme parolası en az 8 karakter olmalı."
        if (encrypt && passphrase !== passphrase2) return "Şifreleme parolaları aynı değil."
        if (!/^[a-z_][a-z0-9_-]{0,31}$/.test(username)) return "Kullanıcı adı küçük harf, rakam, - ve _ olmalı."
        if (!/^[a-zA-Z0-9-]{1,63}$/.test(hostname)) return "Bilgisayar adı harf, rakam ve - olmalı."
        if (password.length === 0) return "Parola boş olamaz."
        if (password !== password2) return "Parolalar aynı değil."
        return ""
    }

    function userFromName(name) {
        const map = { "ç": "c", "ğ": "g", "ı": "i", "ö": "o", "ş": "s", "ü": "u" }
        return name.toLowerCase().split(" ")[0].replace(/[çğıöşü]/g, c => map[c]).replace(/[^a-z0-9_-]/g, "").slice(0, 32)
    }

    function sizeText(bytes) {
        const gb = bytes / 1e9
        return gb >= 1000 ? (gb / 1000).toFixed(1) + " TB" : gb.toFixed(0) + " GB"
    }

    Component.onCompleted: {
        MaterialThemeLoader.reapplyTheme()
        disksProc.running = true
        dotsProc.running = true
        tzProc.running = true
    }

    Process {
        id: disksProc
        command: ["osmium-install", "disks"]
        stdout: StdioCollector {
            onStreamFinished: {
                try { root.disks = JSON.parse(text) } catch (e) { root.disks = [] }
                if (root.disks.length === 1) root.disk = root.disks[0].path
            }
        }
    }
    Process {
        id: dotsProc
        command: ["osmium-dots", "list", "--json"]
        stdout: StdioCollector {
            onStreamFinished: { try { root.builtinDots = JSON.parse(text) } catch (e) {} }
        }
    }
    Process {
        id: tzProc
        command: ["timedatectl", "list-timezones"]
        stdout: StdioCollector {
            onStreamFinished: {
                const list = text.split("\n").filter(l => l.length > 0)
                if (list.length > 0) root.timezones = list
            }
        }
    }
    Process {
        id: searchProc
        property string query: ""
        command: ["osmium-dots", "search", "--json", query]
        stdout: StdioCollector {
            onStreamFinished: {
                try {
                    root.searchResults = JSON.parse(text)
                    root.searchStatus = root.searchResults.length ? "" : "Sonuç yok."
                } catch (e) { root.searchStatus = "Arama başarısız (internet bağlantısı?)." }
            }
        }
    }
    Process {
        id: installProc
        stdinEnabled: true
        command: ["sudo", "-n", "osmium-install", "run"]
        onRunningChanged: {
            if (running) {
                write(root.configText())
                stdinEnabled = false // EOF: osmium-install reads the config until the end
            }
        }
        stdout: SplitParser {
            onRead: line => {
                let m
                try { m = JSON.parse(line) } catch (e) { return }
                if (m.error) {
                    root.errorText = m.error
                } else if (m.done) {
                    root.finished = true
                    root.seconds = m.seconds || 0
                    root.progress = 1
                } else {
                    root.progress = (m.pct || 0) / 100
                    root.progressText = m.msg || ""
                    root.log = root.log.concat([m.msg])
                }
            }
        }
        stderr: SplitParser {
            onRead: line => root.log = root.log.concat([line])
        }
        onExited: (code, status) => {
            root.installing = false
            if (code !== 0 && root.errorText === "") root.errorText = `osmium-install ${code} koduyla çıktı.`
        }
    }

    ColumnLayout {
        anchors { fill: parent; margins: contentPadding }

        RowLayout { // Title bar
            Layout.fillWidth: true
            StyledText {
                Layout.leftMargin: 12
                Layout.fillWidth: true
                text: "OsmiumOS Kurulum"
                color: Appearance.colors.colOnLayer0
                font { family: Appearance.font.family.title; pixelSize: Appearance.font.pixelSize.title; variableAxes: Appearance.font.variableAxes.title }
            }
            RippleButton {
                buttonRadius: Appearance.rounding.full
                implicitWidth: 35; implicitHeight: 35
                enabled: !root.installing
                onClicked: root.close()
                contentItem: MaterialSymbol { anchors.centerIn: parent; horizontalAlignment: Text.AlignHCenter; text: "close"; iconSize: 20 }
            }
        }

        RowLayout {
            Layout.fillWidth: true
            Layout.fillHeight: true
            spacing: contentPadding

            Item {
                Layout.fillHeight: true
                Layout.margins: 5
                implicitWidth: navRail.expanded ? 160 : 56
                NavigationRail {
                    id: navRail
                    anchors { left: parent.left; top: parent.top; bottom: parent.bottom }
                    spacing: 10
                    expanded: root.width > 900
                    NavigationRailTabArray {
                        currentIndex: root.currentPage
                        expanded: navRail.expanded
                        Repeater {
                            model: root.pages
                            NavigationRailButton {
                                required property var index
                                required property var modelData
                                enabled: !root.installing && !root.finished
                                toggled: root.currentPage === index
                                onPressed: root.currentPage = index
                                expanded: navRail.expanded
                                buttonIcon: modelData.icon
                                buttonText: modelData.name
                                showToggledHighlight: false
                            }
                        }
                    }
                    Item { Layout.fillHeight: true }
                }
            }

            Rectangle {
                Layout.fillWidth: true
                Layout.fillHeight: true
                color: Appearance.m3colors.m3surfaceContainerLow
                radius: Appearance.rounding.windowRounding - root.contentPadding
                ColumnLayout {
                    anchors.fill: parent
                    spacing: 0
                    Loader {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        sourceComponent: [welcomePage, diskPage, regionPage, userPage, desktopPage, installPage][root.currentPage]
                    }
                    RowLayout { // Back / next
                        Layout.fillWidth: true
                        Layout.margins: 12
                        visible: !root.installing && !root.finished
                        Item { Layout.fillWidth: true }
                        DialogButton {
                            buttonText: "Geri"
                            enabled: root.currentPage > 0
                            onClicked: root.currentPage--
                        }
                        DialogButton {
                            visible: root.currentPage < root.pages.length - 1
                            buttonText: "İleri"
                            onClicked: root.currentPage++
                        }
                    }
                }
            }
        }
    }

    // ---------- pages ----------

    Component {
        id: welcomePage
        ContentPage {
            forceWidth: true
            ContentSection {
                icon: "rocket_launch"
                title: "OsmiumOS'a hoş geldin"
                StyledText {
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                    color: Appearance.colors.colOnLayer1
                    text: "Birkaç soru soracağız, sonra sistem doğrudan diske açılır: paket indirmez, internet gerekmez. "
                        + "Btrfs + snapper anlık görüntüleri, yapay zekâ kenar çubuğu ve hazır masaüstleri kurulumla gelir."
                }
                NoticeBox {
                    Layout.fillWidth: true
                    materialIcon: "info"
                    text: "Elle bölümleme gerekiyorsa: terminalde sudo calamares"
                }
            }
        }
    }

    Component {
        id: diskPage
        ContentPage {
            forceWidth: true
            ContentSection {
                icon: "hard_drive"
                title: "Kurulacak disk"
                NoticeBox {
                    Layout.fillWidth: true
                    materialIcon: "warning"
                    text: "Seçilen diskteki her şey silinir."
                }
                StyledText {
                    visible: root.disks.length === 0
                    color: Appearance.colors.colOnLayer1
                    text: disksProc.running ? "Diskler aranıyor..." : "Kurulabilecek disk bulunamadı."
                }
                Repeater {
                    model: root.disks
                    StyledRadioButton {
                        required property var modelData
                        Layout.fillWidth: true
                        checked: root.disk === modelData.path
                        onClicked: root.disk = modelData.path
                        text: `${modelData.path}  ${modelData.model || ""}  (${root.sizeText(modelData.size)})`
                        description: (modelData.removable ? "çıkarılabilir, " : "") + (modelData.transport || "")
                    }
                }
            }
            ContentSection {
                icon: "lock"
                title: "Şifreleme"
                ConfigSwitch {
                    buttonIcon: "encrypted"
                    text: "Diski şifrele (LUKS)"
                    checked: root.encrypt
                    onCheckedChanged: root.encrypt = checked
                }
                MaterialTextField {
                    Layout.fillWidth: true
                    visible: root.encrypt
                    placeholderText: "Şifreleme parolası"
                    echoMode: TextInput.Password
                    text: root.passphrase
                    onTextChanged: root.passphrase = text
                }
                MaterialTextField {
                    Layout.fillWidth: true
                    visible: root.encrypt
                    placeholderText: "Şifreleme parolası (tekrar)"
                    echoMode: TextInput.Password
                    text: root.passphrase2
                    onTextChanged: root.passphrase2 = text
                }
            }
        }
    }

    Component {
        id: regionPage
        ContentPage {
            forceWidth: true
            ContentSection {
                icon: "schedule"
                title: "Saat dilimi"
                StyledComboBox {
                    Layout.fillWidth: true
                    model: root.timezones
                    currentIndex: Math.max(0, root.timezones.indexOf(root.timezone))
                    onActivated: index => root.timezone = root.timezones[index]
                }
            }
            ContentSection {
                icon: "translate"
                title: "Dil"
                ConfigSelectionArray {
                    currentValue: root.locale
                    onSelected: v => root.locale = v
                    options: [
                        { displayName: "Türkçe", icon: "language", value: "tr_TR.UTF-8" },
                        { displayName: "English", icon: "language", value: "en_US.UTF-8" },
                    ]
                }
            }
            ContentSection {
                icon: "keyboard"
                title: "Klavye"
                ConfigSelectionArray {
                    currentValue: root.xkbLayout
                    onSelected: v => {
                        root.xkbLayout = v
                        root.keymap = { tr: "trq", us: "us", de: "de-latin1", fr: "fr-latin1" }[v]
                    }
                    options: [
                        { displayName: "Türkçe Q", icon: "keyboard", value: "tr" },
                        { displayName: "English (US)", icon: "keyboard", value: "us" },
                        { displayName: "Deutsch", icon: "keyboard", value: "de" },
                        { displayName: "Français", icon: "keyboard", value: "fr" },
                    ]
                }
            }
        }
    }

    Component {
        id: userPage
        ContentPage {
            forceWidth: true
            ContentSection {
                icon: "person"
                title: "Hesabın"
                MaterialTextField {
                    Layout.fillWidth: true
                    placeholderText: "Adın soyadın"
                    text: root.fullname
                    onTextChanged: {
                        const auto = root.username === "" || root.username === root.userFromName(root.fullname)
                        root.fullname = text
                        if (auto) root.username = root.userFromName(text)
                    }
                }
                MaterialTextField {
                    Layout.fillWidth: true
                    placeholderText: "Kullanıcı adı"
                    text: root.username
                    onTextChanged: root.username = text
                }
                MaterialTextField {
                    Layout.fillWidth: true
                    placeholderText: "Bilgisayarın adı"
                    text: root.hostname
                    onTextChanged: root.hostname = text
                }
                MaterialTextField {
                    Layout.fillWidth: true
                    placeholderText: "Parola"
                    echoMode: TextInput.Password
                    text: root.password
                    onTextChanged: root.password = text
                }
                MaterialTextField {
                    Layout.fillWidth: true
                    placeholderText: "Parola (tekrar)"
                    echoMode: TextInput.Password
                    text: root.password2
                    onTextChanged: root.password2 = text
                }
            }
        }
    }

    Component {
        id: desktopPage
        ContentPage {
            forceWidth: true
            ContentSection {
                icon: "palette"
                title: "Hazır masaüstleri"
                Repeater {
                    model: root.builtinDots
                    StyledRadioButton {
                        required property var modelData
                        Layout.fillWidth: true
                        checked: root.dots === modelData.id
                        onClicked: { root.dots = modelData.id; root.dotsMode = "config" }
                        text: modelData.title
                        description: modelData.description
                    }
                }
            }
            ContentSection {
                icon: "travel_explore"
                title: "İnternetten başka bir dot"
                RowLayout {
                    Layout.fillWidth: true
                    MaterialTextField {
                        id: searchField
                        Layout.fillWidth: true
                        placeholderText: "GitHub'da ara (ör. material, minimal, catppuccin)"
                        onAccepted: searchButton.clicked()
                    }
                    DialogButton {
                        id: searchButton
                        buttonText: "Ara"
                        enabled: searchField.text.length > 0 && !searchProc.running
                        onClicked: {
                            root.searchStatus = "Aranıyor..."
                            searchProc.query = searchField.text
                            searchProc.running = true
                        }
                    }
                }
                StyledText {
                    visible: root.searchStatus !== ""
                    color: Appearance.colors.colSubtext
                    text: root.searchStatus
                }
                Repeater {
                    model: root.searchResults
                    StyledRadioButton {
                        required property var modelData
                        Layout.fillWidth: true
                        checked: root.dots === "github:" + modelData.repo
                        onClicked: root.dots = "github:" + modelData.repo
                        text: `${modelData.repo}  ★${modelData.stars}`
                        description: modelData.description
                    }
                }
                ContentSubsection {
                    visible: root.dots.startsWith("github:")
                    title: "Nasıl kurulsun?"
                    ConfigSelectionArray {
                        currentValue: root.dotsMode
                        onSelected: v => root.dotsMode = v
                        options: [
                            { displayName: "Sadece ayarları kopyala", icon: "content_copy", value: "config" },
                            { displayName: "Kendi betiğini çalıştır", icon: "terminal", value: "script" },
                        ]
                    }
                    NoticeBox {
                        Layout.fillWidth: true
                        visible: root.dotsMode === "script"
                        materialIcon: "warning"
                        text: "Betik ilk girişte bir terminalde gösterilir ve ancak 'çalıştır' yazarsan çalışır. "
                            + "Betikler sudo ile her şeyi yapabilir: yalnızca güvendiğin repoları seç. O zamana kadar ii açılır."
                    }
                }
            }
        }
    }

    Component {
        id: installPage
        ContentPage {
            forceWidth: true
            ContentSection {
                icon: "checklist"
                title: root.finished ? "Kurulum bitti" : "Özet"
                visible: !root.installing
                StyledText {
                    Layout.fillWidth: true
                    wrapMode: Text.Wrap
                    color: Appearance.colors.colOnLayer1
                    visible: !root.finished
                    text: `Disk: ${root.disk || "-"}${root.encrypt ? " (şifreli)" : ""}\n`
                        + `Kullanıcı: ${root.username || "-"} @ ${root.hostname}\n`
                        + `Bölge: ${root.timezone}, ${root.locale}, klavye ${root.xkbLayout}\n`
                        + `Masaüstü: ${root.dots}${root.dots.startsWith("github:") ? " (" + root.dotsMode + ")" : ""}`
                }
                NoticeBox {
                    Layout.fillWidth: true
                    visible: !root.finished && root.problem() !== ""
                    materialIcon: "error"
                    text: root.problem()
                }
                NoticeBox {
                    Layout.fillWidth: true
                    visible: root.errorText !== ""
                    materialIcon: "error"
                    text: root.errorText
                }
                StyledText {
                    visible: root.finished
                    color: Appearance.colors.colOnLayer1
                    text: `OsmiumOS ${root.seconds} saniyede kuruldu. USB'yi çıkarıp yeniden başlat.`
                }
                RowLayout {
                    Item { Layout.fillWidth: true }
                    DialogButton {
                        visible: !root.finished
                        buttonText: root.errorText !== "" ? "Tekrar dene" : "Kur"
                        enabled: root.problem() === ""
                        onClicked: {
                            root.errorText = ""
                            root.log = []
                            root.progress = 0
                            root.installing = true
                            installProc.stdinEnabled = true
                            installProc.running = true
                        }
                    }
                    DialogButton {
                        visible: root.finished
                        buttonText: "Yeniden başlat"
                        onClicked: Quickshell.execDetached(["systemctl", "reboot"])
                    }
                }
            }
            ContentSection {
                icon: "downloading"
                title: "Kuruluyor"
                visible: root.installing
                StyledText {
                    color: Appearance.colors.colOnLayer1
                    text: `${Math.round(root.progress * 100)}%  ${root.progressText}`
                }
                StyledProgressBar {
                    Layout.fillWidth: true
                    value: root.progress
                    wavy: true
                }
                Repeater {
                    model: root.log.slice(-8)
                    StyledText {
                        required property var modelData
                        Layout.fillWidth: true
                        elide: Text.ElideRight
                        color: Appearance.colors.colSubtext
                        font.family: Appearance.font.family.monospace
                        font.pixelSize: Appearance.font.pixelSize.smaller
                        text: modelData
                    }
                }
            }
        }
    }
}
