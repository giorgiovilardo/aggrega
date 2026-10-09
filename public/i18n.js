// Page language: the visitor's first browser language the page speaks,
// otherwise English. English lives in index.html (so the page reads fine
// without JavaScript); other languages replace the elements tagged with
// data-i18n (content), data-i18n-label (aria-label), data-i18n-alt (alt) and
// data-i18n-content (meta content). Translations are trusted, static HTML.

const STRINGS = {
  it: {
    "title": "Aggrega · I tuoi feed, un'edizione",
    "meta.description": "Aggrega è un lettore di feed per desktop veloce e local-first, per RSS, Atom e JSON Feed. Niente account, niente pubblicità, nessun tracciamento, nessuna telemetria, e funziona offline.",
    "skip": "Vai al contenuto",
    "masthead.label": "Aggrega, torna su",
    "tagline": "I tuoi feed · Un'edizione",
    "nav.label": "Sezioni",
    "nav.privacy": "Privacy",
    "nav.features": "Funzioni",
    "nav.download": "Scarica",
    "theme.label": "Tema scuro",
    "front.deck": "Un lettore di feed per desktop, veloce e local-first, con la calma di un quotidiano del mattino. Funziona interamente sul tuo computer e si fa gli affari suoi.",
    "pledges.label": "La privacy in breve",
    "pledge.account": "Niente account",
    "pledge.ads": "Niente pubblicità",
    "pledge.tracking": "Nessun tracciamento",
    "pledge.telemetry": "Nessuna telemetria",
    "pledge.offline": "Funziona offline",
    "cta.get": "Scarica Aggrega",
    "cta.source": "Guarda il codice",
    "front.meta": "Libero e open source · MIT · Linux &amp; macOS",
    "shot.light": "Aggrega con il tema chiaro: una barra laterale di fonti a sinistra e un elenco di articoli in stile giornale con una notizia di apertura.",
    "shot.dark": "Aggrega con il tema scuro.",
    "privacy.dateline": "L'edizione della privacy",
    "privacy.title": "Niente da segnalare",
    "privacy.sub": "Aggrega legge le notizie. Non ne crea su di te.",
    "lead.kicker": "In apertura",
    "lead.title": "Nessuna telemetria. Nemmeno un ping.",
    "lead.deck": "In Aggrega non c'è codice di telemetria, tracciamento o analisi. Né su consenso, né anonimizzato, né “solo report degli arresti anomali.” Non chiama casa, non fa check-in e non ti conta.",
    "lead.byline": "<span class=\"dot\" aria-hidden=\"true\"></span><span class=\"source\" style=\"--src: var(--src-blue)\">Garanzia del prodotto</span> dal primo giorno",
    "ledger.title": "Tutto ciò che Aggrega invia in rete",
    "ledger.feeds": "Richieste per i feed a cui sei iscritto",
    "ledger.thumbs": "Miniature e articoli di quei feed",
    "ledger.lan": "Altre copie di Aggrega nella tua rete locale, solo mentre <em>Impostazioni → Sincronizzazione</em> è aperta",
    "ledger.stats": "Statistiche d'uso, report degli arresti anomali, controllo aggiornamenti",
    "ledger.ads": "Reti pubblicitarie, analisi, “partner”",
    "ledger.server": "Un server di Aggrega, di qualunque tipo",
    "ledger.yes": "Sì",
    "ledger.never": "Mai",
    "ledger.none": "Non esiste",
    "story.account.kicker": "Niente account",
    "story.account.title": "Niente registrazione, niente login, niente cloud.",
    "story.account.body": "Aprilo e inizia a leggere. Iscrizioni, articoli e stato di lettura stanno in un unico file SQLite sul tuo disco, e da nessun'altra parte.",
    "story.tracking.kicker": "Nessun tracciamento",
    "story.tracking.title": "Nessuno ti guarda mentre leggi.",
    "story.tracking.body": "Niente identificatori, niente fingerprinting, nessuna cronologia di lettura che lascia il tuo computer. L'unico a vedere una richiesta è il sito a cui ti sei iscritto.",
    "story.ads.kicker": "Niente pubblicità",
    "story.ads.title": "Sulla pagina c'è solo la notizia.",
    "story.ads.body": "Niente post sponsorizzati, niente fonti promosse, niente upsell. La vista lettura toglie dalla pagina anche menu, barre di condivisione e commenti.",
    "story.offline.kicker": "Funziona offline",
    "story.offline.title": "Leggi in treno. O in aereo.",
    "story.offline.body": "Le notizie e le miniature scaricate restano leggibili senza connessione. Offline, Aggrega te lo dice invece di segnare le fonti come guaste, e si rimette in pari quando torni online.",
    "colophon": "Questa pagina fa quello che predica: niente cookie, niente analisi e nessuna richiesta a terzi. Anche i font sono serviti da qui.",
    "data.dateline": "Dove sono i miei dati?",
    "data.title": "Sul tuo disco",
    "data.sub": "Due cartelle. Tutto qui. Eliminale e Aggrega dimentica tutto.",
    "data.what": "Cosa",
    "data.db": "Iscrizioni, articoli, stato di lettura, impostazioni",
    "data.thumbs": "Cache delle miniature <span class=\"muted\">(si può eliminare)</span>",
    "data.fineprint": "Passi a un altro computer? <strong>Impostazioni → Sincronizzazione</strong> scarica il database di un'altra macchina direttamente dalla rete locale, tra macOS e Linux. Niente resta in ascolto o si annuncia se la scheda Sincronizzazione non è aperta. Importazione ed esportazione <strong>OPML</strong> funzionano con Feedly, Inoreader, NetNewsWire, Miniflux e ogni altro lettore.",
    "features.dateline": "In questa edizione",
    "features.title": "Un lettore come si deve",
    "features.sub": "Piccolo, rapido e silenzioso, con un design preso in prestito dalla redazione.",
    "f.formats.t": "Tutti i formati",
    "f.formats.b": "RSS 0.9x, 1.0 e 2.0, Atom e JSON Feed, da tutte le fonti che vuoi.",
    "f.add.t": "“Aggiungi fonte” intelligente",
    "f.add.b": "Incolla l'URL di un feed, o anche solo <code>theverge.com</code>. Aggrega trova il feed per te.",
    "f.reader.t": "Vista lettura",
    "f.reader.b": "Una colonna pulita con titolo, foto e testo. Per i feed con solo il riassunto arriva l'articolo completo, salvato per la lettura offline.",
    "f.refresh.t": "Aggiorna con garbo",
    "f.refresh.b": "In parallelo, in background, quando vuoi tu. Con le richieste condizionali i feed invariati non costano quasi nulla.",
    "f.sync.t": "Sincronizzazione in LAN e OPML",
    "f.sync.b": "Porta le tue iscrizioni da qualsiasi lettore e spostale tra computer senza un cloud in mezzo.",
    "f.theme.t": "Chiaro e scuro",
    "f.theme.b": "Carta di giornale o inchiostro dell'ultima edizione. Segue il desktop, oppure scegline uno e lo ricorda.",
    "f.light.t": "Leggero sulle risorse",
    "f.light.b": "Un unico eseguibile Rust nativo. Nessun motore di browser dentro, e zero CPU quando è inattivo.",
    "f.keys.t": "Amico della tastiera",
    "f.keys.b": "<kbd>F5</kbd> per aggiornare, <kbd>O</kbd> per aprire l'originale, <kbd>M</kbd> per segnare come letto, <kbd>Esc</kbd> per tornare indietro.",
    "spot.alt": "La finestra Aggiungi fonte, con un campo per l'indirizzo di un sito o di un feed.",
    "spot.kicker": "Aggiungi fonte",
    "spot.title": "Basta scrivere il sito.",
    "spot.body": "Aggrega cerca il feed nei tag <code>&lt;link rel=\"alternate\"&gt;</code> della pagina o in percorsi comuni come <code>/feed</code> e <code>/rss.xml</code>. Arrivi da un altro lettore? <strong>Importa da OPML</strong> è proprio nella schermata di benvenuto.",
    "dl.dateline": "La tua copia",
    "dl.title": "Libero, aperto, tuo",
    "dl.sub": "Licenza MIT. Nessun account da creare prima di provarlo.",
    "dl.arch.title": "Compilalo in un minuto",
    "dl.copy.label": "Copia i comandi",
    "dl.arch.note": "Installa un vero pacchetto con voce nel launcher e icona. Altre distribuzioni: vedi <a href=\"https://github.com/moebiusmania/aggrega/blob/main/docs/BUILDING.md\">la guida alla compilazione</a>.",
    "dl.mac.title": "Apple silicon, macOS 11+",
    "dl.mac.body": "Ogni release produce <code>Aggrega.app</code> in un'immagine disco. L'app non è notarizzata, quindi al primo avvio serve <strong>Apri comunque</strong> in <em>Impostazioni di Sistema → Privacy e sicurezza</em>.",
    "dl.mac.latest": "Ultime build",
    "dl.mac.build": "Compilalo tu",
    "footer.tag": "L'edizione è chiusa",
    "footer.license": "Rilasciato con <a href=\"https://github.com/moebiusmania/aggrega/blob/main/LICENSE\">licenza MIT</a>. Realizzato con Rust e <a href=\"https://slint.dev\" rel=\"noopener\">Slint</a>. Composto in Source Serif 4 e Libre Franklin (<a href=\"fonts/OFL.txt\">SIL OFL 1.1</a>).",
    "footer.links": "<a href=\"https://github.com/moebiusmania/aggrega\">GitHub</a> · <a href=\"https://github.com/moebiusmania/aggrega/issues\">Segnala un problema</a> · <a href=\"https://github.com/moebiusmania/aggrega/blob/main/CONTRIBUTING.md\">Contribuisci</a>",
    "copy": "Copia",
    "copied": "Copiato",
    "copy.fallback": "Seleziona e copia"
  },
};

const LANG =
  (navigator.languages ?? [navigator.language])
    .map((l) => String(l).toLowerCase().split("-")[0])
    .find((l) => l === "en" || l in STRINGS) ?? "en";

// The current language's text for `key`, or `fallback` (the English).
const t = (key, fallback) => STRINGS[LANG]?.[key] ?? fallback;

if (LANG !== "en") {
  document.documentElement.lang = LANG;
  const apply = (attr, set) => {
    for (const el of document.querySelectorAll(`[${attr}]`)) {
      const text = STRINGS[LANG][el.getAttribute(attr)];
      if (text !== undefined) set(el, text);
    }
  };
  apply("data-i18n", (el, s) => (el.innerHTML = s));
  apply("data-i18n-label", (el, s) => el.setAttribute("aria-label", s));
  apply("data-i18n-alt", (el, s) => (el.alt = s));
  apply("data-i18n-content", (el, s) => (el.content = s));
}
