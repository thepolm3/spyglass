use std::path::PathBuf;

use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::QueryParser;
use tantivy::{schema::*, DocAddress};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy};

type Score = f32;
type SearchResult = (Score, DocAddress);

pub enum IndexPath {
    // Directory
    LocalPath(PathBuf),
    // In memory index for testing purposes.
    Memory,
}

pub struct Searcher {
    index: Index,
    schema: Schema,
    reader: IndexReader,
    writer: IndexWriter,
}

impl Searcher {
    pub fn with_index(index_path: &IndexPath) -> Self {
        let mut schema_builder = Schema::builder();

        // Our first field is title. We want:
        // - full-text search and
        // - to retrieve the document after the search
        // TEXT: Means the field should be tokenized and indexed, along with its term
        //      frequency and term positions.
        // STORED: Means that the field will also be saved in a compressed, row oriented
        //      key-value store. This store is useful to reconstruct the documents that
        //      were selected during the search phase.
        schema_builder.add_text_field("title", TEXT | STORED);
        // Indexed but don't store for retreival
        schema_builder.add_text_field("body", TEXT);

        let schema = schema_builder.build();
        let index = match index_path {
            IndexPath::LocalPath(path) => {
                let dir = MmapDirectory::open(path).unwrap();
                Index::open_or_create(dir, schema.clone()).unwrap()
            }
            IndexPath::Memory => Index::create_in_ram(schema.clone()),
        };

        // Should only be one writer at a time. This single IndexWriter is already
        // multithreaded.
        let writer = index
            .writer(50_000_000)
            .expect("Unable to create index_writer");

        // For a search server you will typically create on reader for the entire
        // lifetime of your program.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommit)
            .try_into()
            .expect("Unable to create reader");

        Searcher {
            index,
            schema,
            reader,
            writer,
        }
    }

    pub fn add_document(&mut self, title: &str, body: &str) -> tantivy::Result<()> {
        let title_field = self.schema.get_field("title").unwrap();
        let body_field = self.schema.get_field("body").unwrap();

        let mut doc = Document::default();
        doc.add_text(title_field, title);
        doc.add_text(body_field, body);
        self.writer.add_document(doc);

        self.writer.commit()?;

        Ok(())
    }

    pub fn search(&self, query_string: &str) -> Vec<SearchResult> {
        let searcher = self.reader.searcher();

        let title = self.schema.get_field("title").unwrap();
        let body = self.schema.get_field("body").unwrap();

        let query_parser = QueryParser::for_index(&self.index, vec![title, body]);
        let query = query_parser
            .parse_query(query_string)
            .expect("Unable to parse query");

        println!("searching...");
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(10))
            .expect("Unable to execute query");

        log::info!(
            "query `{}` returned {} results",
            query_string,
            top_docs.len()
        );
        top_docs.into_iter().collect()
    }
}

#[cfg(test)]
mod test {
    use crate::search::{IndexPath, Searcher};

    #[test]
    pub fn test_indexer() {
        let mut searcher = Searcher::with_index(&IndexPath::Memory);

        searcher
            .add_document(
                "Of Mice and Men",
                "A few miles south of Soledad, the Salinas River drops in close to the hillside
            bank and runs deep and green. The water is warm too, for it has slipped twinkling
            over the yellow sands in the sunlight before reaching the narrow pool. On one
            side of the river the golden foothill slopes curve up to the strong and rocky
            Gabilan Mountains, but on the valley side the water is lined with trees—willows
            fresh and green with every spring, carrying in their lower leaf junctures the
            debris of the winter’s flooding; and sycamores with mottled, white, recumbent
            limbs and branches that arch over the pool",
            )
            .expect("Unable to add doc");

        searcher
            .add_document(
                "Of Mice and Men",
                "A few miles south of Soledad, the Salinas River drops in close to the hillside
            bank and runs deep and green. The water is warm too, for it has slipped twinkling
            over the yellow sands in the sunlight before reaching the narrow pool. On one
            side of the river the golden foothill slopes curve up to the strong and rocky
            Gabilan Mountains, but on the valley side the water is lined with trees—willows
            fresh and green with every spring, carrying in their lower leaf junctures the
            debris of the winter’s flooding; and sycamores with mottled, white, recumbent
            limbs and branches that arch over the pool",
            )
            .expect("Unable to add doc");
        searcher
            .add_document(
                "Frankenstein: The Modern Prometheus",
                "You will rejoice to hear that no disaster has accompanied the commencement of an
             enterprise which you have regarded with such evil forebodings.  I arrived here
             yesterday, and my first task is to assure my dear sister of my welfare and
             increasing confidence in the success of my undertaking.",
            )
            .expect("Unable to add doc");

        // add a small delay so that the documents can be properly committed
        std::thread::sleep(std::time::Duration::from_millis(100));

        let results = searcher.search("gabilan mountains");
        assert_eq!(results.len(), 2);
    }
}