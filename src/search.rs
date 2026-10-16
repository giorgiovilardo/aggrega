//! Article search, as pure logic: plain text of article bodies, folding of
//! letters, and the query rewriting that gives typo tolerance. The SQL that
//! runs the queries lives in `db.rs`, over the `articles_fts` index.

use crate::reader::{self, Block, Kind};

/// Readable text of `blocks`, one space between blocks. Code and images are
/// skipped because they are not prose a reader would search for.
pub fn plain_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter(|b| {
            matches!(
                b.kind,
                Kind::Heading | Kind::Paragraph | Kind::Quote | Kind::Bullet
            )
        })
        .map(|b| b.text.as_str())
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Plain text of an article body stored in the encoded line format.
pub fn body_text(encoded: &str) -> String {
    plain_text(&reader::decode(encoded))
}

/// Lowercases and folds the way the index does, so that a query and a word
/// compare equal across accents and case. Combining marks are dropped and
/// Latin/Cyrillic letters that decompose are mapped to their base letter.
/// Letters with no decomposition are mapped by hand, and ß, æ and œ expand
/// to two letters, so `fold("Straße") == "strasse"`.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        match c {
            '\u{300}'..='\u{36f}' => {}
            'ß' => out.push_str("ss"),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            _ => out.push(base_letter(c)),
        }
    }
    out
}

fn base_letter(c: char) -> char {
    match c {
        'à'..='å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' | 'đ' => 'd',
        'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ţ' | 'ť' | 'ŧ' => 't',
        'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        // Cyrillic: ё and й decompose to е and и.
        '\u{451}' => '\u{435}',
        '\u{439}' => '\u{438}',
        _ => c,
    }
}

/// Edit budget for a query token of `len` folded characters: none for short
/// tokens (prefix only), one edit for 4-7 characters, two from 8 characters.
fn max_edits(len: usize) -> usize {
    match len {
        0..=3 => 0,
        4..=7 => 1,
        _ => 2,
    }
}

/// Optimal string alignment distance (Damerau-Levenshtein with adjacent
/// transpositions, no substring edited twice), or `None` when it is more
/// than `max`. Gives up as soon as a whole row is over `max`.
fn osa_distance(a: &[char], b: &[char], max: usize) -> Option<usize> {
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > max {
        return None;
    }
    let mut prev2 = vec![0usize; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
            row_min = row_min.min(v);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[m] <= max).then_some(prev[m])
}

/// The index vocabulary, folded for matching. Built from
/// `Store::search_terms` and kept between queries.
// used by the search UI, next step
#[allow(dead_code)]
pub struct Terms {
    /// `(folded chars, original term)`, sorted by the folded form.
    entries: Vec<(Vec<char>, String)>,
}

impl Terms {
    // used by the search UI, next step
    #[allow(dead_code)]
    pub fn new(terms: impl IntoIterator<Item = String>) -> Self {
        let mut entries: Vec<(Vec<char>, String)> = terms
            .into_iter()
            .map(|t| (fold(&t).chars().collect(), t))
            .collect();
        entries.sort();
        Self { entries }
    }

    // used by the search UI, next step
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Most fuzzy alternatives kept for one query token.
const MAX_FUZZY: usize = 32;

/// A compiled query: one FTS5 MATCH expression per tier, best first.
///
/// Tier k matches the words that need up to k "steps" of fuzziness: the
/// cumulative expression E_k ANDs, per token, its exact and prefix forms and
/// the vocabulary terms within `k` edits. A document lands in the tier of its
/// weakest token, so the tiers never overlap. Each later tier is written as
/// `(E_k) NOT (E_prev)`, and a tier whose expression equals the previous one
/// is left out.
pub struct Query {
    tiers: Vec<String>,
}

impl Query {
    // used by the search UI, next step
    #[allow(dead_code)]
    pub fn tiers(&self) -> &[String] {
        &self.tiers
    }
}

/// Compiles a free-text query. `None` when it has no tokens at all. Without
/// `terms` there is only one tier: exact words, and a prefix for the last one.
// used by the search UI, next step
#[allow(dead_code)]
pub fn compile(q: &str, terms: Option<&Terms>) -> Option<Query> {
    let lower = q.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let last = tokens.len() - 1;
    // Each token's vocabulary neighbours, computed once against its own edit
    // budget and sorted by distance then term. The cap applies to this list,
    // so every tier takes a prefix of the same candidates.
    let fuzzy: Vec<Vec<(usize, &str)>> = tokens
        .iter()
        .map(|tok| {
            let folded: Vec<char> = fold(tok).chars().collect();
            let max = max_edits(folded.len());
            let mut near: Vec<(usize, &str)> = terms.map_or_else(Vec::new, |t| {
                t.entries
                    .iter()
                    .filter_map(|(f, orig)| {
                        osa_distance(&folded, f, max).map(|d| (d, orig.as_str()))
                    })
                    .collect()
            });
            near.sort_unstable();
            near.truncate(MAX_FUZZY);
            near
        })
        .collect();

    let mut tiers: Vec<String> = Vec::new();
    let mut prev: Option<String> = None;
    for level in 0..=2 {
        let mut clauses = Vec::with_capacity(tokens.len());
        for (i, tok) in tokens.iter().enumerate() {
            let mut alts = vec![quote(tok)];
            if i == last {
                alts.push(format!("{}*", quote(tok)));
            }
            for (dist, term) in &fuzzy[i] {
                let alt = quote(term);
                if *dist <= level && !alts.contains(&alt) {
                    alts.push(alt);
                }
            }
            clauses.push(format!("({})", alts.join(" OR ")));
        }
        let expr = clauses.join(" AND ");
        match prev {
            None => tiers.push(expr.clone()),
            Some(ref p) if *p == expr => continue,
            Some(ref p) => tiers.push(format!("({expr}) NOT ({p})")),
        }
        prev = Some(expr);
    }
    Some(Query { tiers })
}

/// Quoted FTS5 string. Embedded quotes are doubled.
fn quote(t: &str) -> String {
    format!("\"{}\"", t.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist(a: &str, b: &str, max: usize) -> Option<usize> {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        osa_distance(&a, &b, max)
    }

    fn terms(words: &[&str]) -> Terms {
        Terms::new(words.iter().map(|w| w.to_string()))
    }

    fn tiers(q: &str, t: Option<&Terms>) -> Vec<String> {
        compile(q, t).expect("query has tokens").tiers().to_vec()
    }

    #[test]
    fn osa_distance_cases() {
        assert_eq!(dist("rust", "rust", 0), Some(0));
        assert_eq!(dist("rsut", "rust", 1), Some(1)); // transposition
        assert_eq!(dist("rsut", "rust", 0), None);
        assert_eq!(dist("sqlate", "sqlite", 1), Some(1)); // substitution
        assert_eq!(dist("electrc", "electric", 1), Some(1)); // deletion
        assert_eq!(dist("konferez", "konferenz", 1), Some(1)); // one insertion
        assert_eq!(dist("konferez", "konferenz", 0), None);
        // OSA: "ca" -> "abc" is 3 (true Damerau would be 2).
        assert_eq!(dist("ca", "abc", 2), None);
        assert_eq!(dist("kitten", "sitting", 3), Some(3));
        assert_eq!(dist("kitten", "sitting", 2), None);
        assert_eq!(dist("ab", "abcdef", 3), None); // length cutoff
        assert_eq!(dist("", "ab", 2), Some(2));
    }

    #[test]
    fn fold_lowercases_and_drops_marks() {
        assert_eq!(fold("Perché"), "perche");
        assert_eq!(fold("FRANÇAIS"), "francais");
        assert_eq!(fold("Ångström"), "angstrom");
        assert_eq!(fold("учёные"), "ученые");
    }

    #[test]
    fn fold_expands_and_maps_undecomposed_letters() {
        assert_eq!(fold("Straße"), "strasse");
        assert_eq!(fold("Łódź"), "lodz");
        assert_eq!(fold("Øre"), "ore");
        assert_eq!(fold("Æble Œuvre"), "aeble oeuvre");
        assert_eq!(fold("đ ħ ŧ ı"), "d h t i");
    }

    #[test]
    fn max_edits_by_length() {
        assert_eq!(max_edits(3), 0);
        assert_eq!(max_edits(4), 1);
        assert_eq!(max_edits(7), 1);
        assert_eq!(max_edits(8), 2);
    }

    #[test]
    fn plain_text_skips_code_and_images() {
        let blocks = vec![
            Block::new(Kind::Heading, "Title"),
            Block::new(Kind::Paragraph, "First."),
            Block::new(Kind::Code, "let x = 1;"),
            Block::new(Kind::Image, "https://example.com/a.png"),
            Block::new(Kind::Quote, "Quoted."),
            Block::new(Kind::Bullet, "Point"),
            Block::new(Kind::Paragraph, "   "),
        ];
        assert_eq!(plain_text(&blocks), "Title First. Quoted. Point");
        assert_eq!(
            body_text(&reader::encode(&blocks)),
            "Title First. Quoted. Point"
        );
    }

    #[test]
    fn compile_without_tokens_is_none() {
        assert!(compile("", None).is_none());
        assert!(compile(" !? ", None).is_none());
    }

    #[test]
    fn compile_without_vocabulary_has_one_tier() {
        assert_eq!(tiers("rust", None), ["(\"rust\" OR \"rust\"*)"]);
        assert_eq!(
            tiers("night service", None),
            ["(\"night\") AND (\"service\" OR \"service\"*)"]
        );
    }

    #[test]
    fn folded_spelling_finds_the_original_word() {
        let t = terms(&["straße", "rust"]);
        let first = &tiers("strasse", Some(&t))[0];
        assert!(first.contains("\"straße\""), "{first}");
    }

    #[test]
    fn typo_neighbours_come_in_the_next_tier() {
        let t = terms(&["sqlite", "rust"]);
        let all = tiers("sqlate", Some(&t));
        assert_eq!(all.len(), 2);
        assert!(!all[0].contains("sqlite"));
        assert!(all[1].contains("\"sqlite\""));
        assert!(
            all[1].ends_with(&format!(" NOT ({})", all[0])),
            "{}",
            all[1]
        );
    }

    #[test]
    fn short_tokens_never_get_fuzzy_neighbours() {
        let t = terms(&["rust"]);
        let all = tiers("rst", Some(&t));
        assert!(all.iter().all(|tier| !tier.contains("rust")), "{all:?}");
    }

    #[test]
    fn identical_tiers_are_not_repeated() {
        let t = terms(&["rust"]);
        assert_eq!(tiers("rust", Some(&t)).len(), 1);
    }

    #[test]
    fn corpus_is_consistent() {
        let docs = corpus::docs();
        let cases = corpus::cases();
        assert!(docs.len() >= 30, "corpus has {} docs", docs.len());
        assert!(cases.len() >= 30, "corpus has {} cases", cases.len());

        let mut ids: Vec<i64> = docs.iter().map(|d| d.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), docs.len(), "doc ids must be unique");

        for c in &cases {
            assert!(
                !c.expect.is_empty(),
                "case {:?} has no expected ids",
                c.query
            );
            for id in c.expect {
                assert!(
                    ids.contains(id),
                    "case {:?} expects missing doc {id}",
                    c.query
                );
            }
        }
    }

    #[test]
    fn score_requires_every_expected_id() {
        let c = corpus::Case {
            query: "x",
            expect: &[1, 2],
            kind: "exact",
        };
        assert!(corpus::score(&[2, 1, 9], &c));
        assert!(!corpus::score(&[1], &c));
    }
}

#[cfg(test)]
pub mod corpus {
    /// One article of the corpus. Fields are plain strings: `db` tests store them as a feed.
    pub struct Doc {
        pub id: i64,
        pub title: String,
        pub snippet: String,
        pub body: String,
    }

    /// A query and the ids that a correct search must return for it.
    pub struct Case {
        pub query: &'static str,
        pub expect: &'static [i64],
        pub kind: &'static str,
    }

    fn doc(id: i64, title: &str, snippet: &str, body: &str) -> Doc {
        Doc {
            id,
            title: title.to_string(),
            snippet: snippet.to_string(),
            body: body.to_string(),
        }
    }

    pub fn docs() -> Vec<Doc> {
        vec![
            // English
            doc(
                1,
                "Rust 2026 release brings faster compile times",
                "The Rust project shipped its new edition with incremental build improvements. Developers report noticeably shorter waits.",
                "The release focuses on compiler performance and smaller binaries. Maintainers say the linker changes alone cut link times on large projects.",
            ),
            doc(
                2,
                "Local-first apps gain ground among developers",
                "More teams are keeping data on the device and syncing later. The approach trades cloud convenience for privacy and speed.",
                "Offline-friendly software has become a popular design goal. Engineers argue that storing data locally makes apps feel instant.",
            ),
            doc(
                3,
                "How to read a financial statement in ten minutes",
                "A short guide to balance sheets, income statements and cash flow. It explains the numbers investors check first.",
                "Start with revenue and net margin before looking at debt. The article includes a checklist you can reuse for any company.",
            ),
            doc(
                4,
                "Garden tips for a dry summer",
                "Mulch and deep watering keep roots cool when rain is scarce. Choose drought tolerant plants for the sunniest beds.",
                "Water early in the morning to reduce evaporation. Collect rainwater in barrels for the tomatoes and herbs.",
            ),
            doc(
                5,
                "Electric bikes overtake cars in city sales",
                "Urban riders are switching to e-bikes for short commutes. Dealers report record demand this quarter.",
                "Battery prices have fallen, making models cheaper than ever. Cities are also building wider lanes for cyclists.",
            ),
            doc(
                6,
                "Why SQLite is a good choice for small servers",
                "A single file database removes the need for a separate process. Benchmarks show it handles read heavy workloads well.",
                "Write-ahead logging allows concurrent readers during updates. The author recommends it for blogs and internal tools.",
            ),
            doc(
                7,
                "Museums reopen their historic wings to visitors",
                "The gallery restored its nineteenth century rooms after a three year project. Admission is free on the first Sunday.",
                "Curators installed new lighting to protect fragile paintings. Visitors can also see a temporary exhibition on maritime maps.",
            ),
            doc(
                8,
                "Commuter trains add night service on weekends",
                "The regional operator will run extra services after midnight. Officials hope the change reduces taxi traffic downtown.",
                "The timetable starts next month on the northern line. Tickets bought in advance will be valid on every new departure.",
            ),
            // Italian
            doc(
                9,
                "Come cambiano le città italiane",
                "Piazze pedonali e piste ciclabili stanno ridisegnando i centri storici. Il dibattito coinvolge commercianti e residenti.",
                "Molti sindaci sostengono che l'aria sia più pulita con meno auto. Alcuni negozi temono però un calo delle vendite.",
            ),
            doc(
                10,
                "Ricette veloci per la cena di domenica",
                "Un primo di pasta fresca e un secondo di pesce bastano per sei persone. Il menu si prepara in meno di due ore.",
                "Il segreto è la salsa di pomodoro fatta con pomodori maturi. Per il dolce basta una crostata con marmellata di albicocche.",
            ),
            doc(
                11,
                "Nuovi collegamenti ferroviari per il Sud",
                "La linea veloce collegherà Napoli e Reggio in meno di quattro ore. I lavori sono partiti questa primavera.",
                "Il progetto prevede stazioni rinnovate e nuovi treni regionali. Le autorità assicurano che i biglietti resteranno invariati.",
            ),
            doc(
                12,
                "Gli articoli scientifici sul clima diventano più leggibili",
                "Una nuova piattaforma riassume gli articoli in linguaggio semplice. I ricercatori la considerano utile per i giornalisti.",
                "Il sistema usa modelli linguistici per estrarre i punti chiave. Gli editor possono correggere le sintesi prima della pubblicazione.",
            ),
            doc(
                13,
                "Perché il caffè costa sempre di più",
                "Il prezzo dei chicchi è salito dopo due raccolti difficili. Le torrefazioni cercano nuove origini in Africa.",
                "Gli esperti spiegano che la siccità ha colpito le piantagioni in Brasile. Il consumatore, però, può risparmiare comprando in grani.",
            ),
            // French
            doc(
                14,
                "Le marché du livre d'occasion séduit les lecteurs",
                "Les librairies en ligne proposent désormais des éditions anciennes. Les prix restent bas pour les romans classiques.",
                "Beaucoup de lecteurs cherchent des livres introuvables en magasin. Les bibliothèques organisent aussi des bourses d'échange.",
            ),
            doc(
                15,
                "Le français reste la langue de la diplomatie",
                "Malgré la domination de l'anglais, le français garde une place forte dans les organisations internationales.",
                "Les ambassades forment encore leurs diplomates dans cette langue. Des universités d'Afrique voient leurs inscriptions augmenter.",
            ),
            doc(
                16,
                "Une nouvelle ligne de tramway traverse Lyon",
                "Les travaux ont duré trois ans dans le quartier de la Part-Dieu. Le service débute lundi matin.",
                "La ligne relie la gare aux rives du Rhône en vingt minutes. Les habitants espèrent moins de bouchons au centre-ville.",
            ),
            doc(
                17,
                "Les jardins partagés se multiplient dans Paris",
                "Des associations transforment les toits et les friches en potagers collectifs. Les voisins se partagent les récoltes.",
                "La mairie finance les premiers outils et la terre. Certains projets accueillent aussi des ruches et des poules.",
            ),
            doc(
                18,
                "Le cloud souverain, un enjeu pour les entreprises",
                "Les données sensibles doivent rester sur des serveurs européens. Les start-up cherchent des hébergeurs locaux fiables.",
                "Le règlement sur la protection des données renforce cette exigence. Les grands fournisseurs américains proposent désormais des offres régionales.",
            ),
            // German
            doc(
                19,
                "Die Straße zur Donau wird endlich saniert",
                "Die Bauarbeiten auf der Bundesstraße beginnen im April. Pendler müssen mit Umleitungen rechnen.",
                "Die Brücke über den Fluss wird komplett erneuert. Die Kosten liegen bei rund vierzig Millionen Euro.",
            ),
            doc(
                20,
                "Winterharte Pflanzen für den Balkon",
                "Immergrüne Sträucher und Gräser halten auch bei Frost die Farbe. Wer im Herbst pflanzt, hat im Frühjahr die meisten Blüten.",
                "Der Boden sollte mit Laub abgedeckt werden. Töpfe brauchen im Winter einen geschützten Platz an der Hauswand.",
            ),
            doc(
                21,
                "Neue Festkörperbatterien versprechen größere Reichweite",
                "Forscher haben einen Prototyp vorgestellt, der deutlich schneller lädt. Die Serienfertigung soll in fünf Jahren starten.",
                "Die Technik verzichtet auf brennbare Flüssigkeiten im Inneren. Autohersteller prüfen bereits erste Testfahrzeuge.",
            ),
            doc(
                22,
                "Konferenz über Open-Source-Software in Berlin",
                "Entwickler diskutieren über Sicherheit und Finanzierung von Projekten. Die Veranstaltung findet im Mai statt.",
                "Mehrere Stiftungen kündigten neue Förderprogramme an. Die Vorträge werden anschließend online veröffentlicht.",
            ),
            // Spanish
            doc(
                23,
                "El niño que cruzó el océano en kayak",
                "Un adolescente de doce años completó el trayecto con ayuda de su familia. Su historia se volvió viral en las redes.",
                "El viaje duró casi tres meses por la costa atlántica. Los organizadores piden más seguridad para los menores en el mar.",
            ),
            doc(
                24,
                "Cómo ahorrar energía en casa durante el invierno",
                "Cambiar las bombillas y sellar las ventanas reduce mucho la factura. Pequeños hábitos también cuentan.",
                "Bajar la calefacción un grado puede ahorrar hasta un siete por ciento. Las cortinas gruesas ayudan a conservar el calor por la noche.",
            ),
            doc(
                25,
                "La selección española prepara su próximo torneo",
                "El equipo entrena en Madrid antes de viajar al extranjero. El entrenador confía en la juventud de la plantilla.",
                "Varios jugadores jóvenes debutarán en la fase de grupos. La federación espera llenar el estadio en los primeros partidos.",
            ),
            doc(
                26,
                "Nuevas rutas de senderismo en los Pirineos",
                "Ocho itinerarios señalizados recorren valles y lagos de alta montaña. Los guías recomiendan llevar agua y calzado adecuado.",
                "Algunas rutas exigen experiencia en terreno nevado. El parque nacional ha publicado mapas actualizados en papel y en línea.",
            ),
            // Russian
            doc(
                27,
                "Новый мост через Волгу откроют осенью",
                "Строительство моста заняло четыре года и обошлось в несколько миллиардов рублей. Движение начнется в октябре.",
                "Мост снимет нагрузку с центральных улиц города. Водители смогут объехать пробки на окраинах.",
            ),
            doc(
                28,
                "Учёные нашли новый вид морских черепах",
                "Находка сделана у побережья Австралии на глубине двадцати метров. Исследователи описали животное в научном журнале.",
                "Черепахи отличаются от известных видов формой панциря. Команда планирует вернуться в регион следующим летом.",
            ),
            doc(
                29,
                "Как выбрать ноутбук для программирования",
                "Важны клавиатура, объем памяти и время работы от батареи. Для компиляции больших проектов нужен быстрый процессор.",
                "Многие разработчики предпочитают Linux на легких машинах. Перед покупкой стоит проверить экран и охлаждение.",
            ),
            doc(
                30,
                "Городской транспорт переходит на электрические автобусы",
                "Первые двадцать машин выйдут на маршруты в начале зимы. Оператор обещает тихую работу и нулевые выбросы.",
                "Зарядные станции установят на конечных остановках. Пассажиры оценили плавный ход и кондиционеры в салоне.",
            ),
        ]
    }

    pub fn cases() -> Vec<Case> {
        vec![
            // exact
            Case {
                query: "rust",
                expect: &[1],
                kind: "exact",
            },
            Case {
                query: "sqlite",
                expect: &[6],
                kind: "exact",
            },
            Case {
                query: "garden",
                expect: &[4],
                kind: "exact",
            },
            Case {
                query: "museums",
                expect: &[7],
                kind: "exact",
            },
            Case {
                query: "night service",
                expect: &[8],
                kind: "exact",
            },
            Case {
                query: "ricette",
                expect: &[10],
                kind: "exact",
            },
            Case {
                query: "caffè",
                expect: &[13],
                kind: "exact",
            },
            Case {
                query: "lecteurs",
                expect: &[14],
                kind: "exact",
            },
            Case {
                query: "Straße",
                expect: &[19],
                kind: "exact",
            },
            Case {
                query: "niño",
                expect: &[23],
                kind: "exact",
            },
            // diacritics
            Case {
                query: "perche",
                expect: &[13],
                kind: "diacritics",
            },
            Case {
                query: "francais",
                expect: &[15],
                kind: "diacritics",
            },
            Case {
                query: "strasse",
                expect: &[19],
                kind: "diacritics",
            },
            Case {
                query: "nino",
                expect: &[23],
                kind: "diacritics",
            },
            // typo-substitution
            Case {
                query: "sqlate",
                expect: &[6],
                kind: "typo-substitution",
            },
            Case {
                query: "tramvay",
                expect: &[16],
                kind: "typo-substitution",
            },
            Case {
                query: "Pirineas",
                expect: &[26],
                kind: "typo-substitution",
            },
            // typo-transposition
            Case {
                query: "rsut",
                expect: &[1],
                kind: "typo-transposition",
            },
            Case {
                query: "gardne",
                expect: &[4],
                kind: "typo-transposition",
            },
            Case {
                query: "musuems",
                expect: &[7],
                kind: "typo-transposition",
            },
            // typo-missing
            Case {
                query: "electrc bikes",
                expect: &[5],
                kind: "typo-missing",
            },
            Case {
                query: "Konferez",
                expect: &[22],
                kind: "typo-missing",
            },
            Case {
                query: "jardns",
                expect: &[17],
                kind: "typo-missing",
            },
            // inflection
            Case {
                query: "articolo",
                expect: &[12],
                kind: "inflection",
            },
            Case {
                query: "Konferenzen",
                expect: &[22],
                kind: "inflection",
            },
            Case {
                query: "черепаху",
                expect: &[28],
                kind: "inflection",
            },
            // prefix
            Case {
                query: "artic",
                expect: &[12],
                kind: "prefix",
            },
            Case {
                query: "Pirin",
                expect: &[26],
                kind: "prefix",
            },
            Case {
                query: "electr",
                expect: &[5],
                kind: "prefix",
            },
            // cyrillic
            Case {
                query: "мост",
                expect: &[27],
                kind: "cyrillic",
            },
            Case {
                query: "ноутбук",
                expect: &[29],
                kind: "cyrillic",
            },
            Case {
                query: "учёные",
                expect: &[28],
                kind: "cyrillic",
            },
            Case {
                query: "Волгу",
                expect: &[27],
                kind: "cyrillic",
            },
        ]
    }

    /// True if every expected id is among `results`. Recall only: extra
    /// results do not count against a case.
    pub fn score(results: &[i64], c: &Case) -> bool {
        c.expect.iter().all(|id| results.contains(id))
    }
}
