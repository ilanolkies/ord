use {
  super::*,
  crate::index::entry::InscriptionEntry,
  indicatif::{ProgressIterator, ProgressStyle},
  log::log_enabled,
};

#[derive(Debug, Parser)]
pub(crate) struct Export {
  #[clap(
    long,
    default_value = "inscriptions",
    help = "Write to directory <OUTPUT_DIR>"
  )]
  output_dir: PathBuf,
}

impl Export {
  pub(crate) fn run(&self, options: Options) -> Result {
    let index = Arc::new(Index::open(&options)?);
    index.update()?;
    self.run_with_index(index)?;
    Ok(())
  }

  fn run_with_index(&self, index: Arc<Index>) -> Result {
    self.prepare_directory_tree()?;
    let written_numbers = self.get_written_numbers()?;
    let ids = index.get_inscriptions(None)?;
    for id in Export::add_progress_bar("exporting inscriptions", ids.into_values()) {
      let entry = index
        .get_inscription_entry(id)?
        .ok_or_else(|| anyhow!("inscription entry not found: {id}"))?;
      if !written_numbers.contains(&entry.number) {
        self.export_inscription_by_id(&index, id, entry)?;
      }
      if SHUTTING_DOWN.load(atomic::Ordering::Relaxed) {
        break;
      }
    }
    Ok(())
  }

  fn prepare_directory_tree(&self) -> Result {
    fs::create_dir_all(self.numbers_dir())?;
    Ok(())
  }

  fn numbers_dir(&self) -> PathBuf {
    self.output_dir.join("numbers")
  }

  fn get_written_numbers(&self) -> Result<HashSet<u64>> {
    Ok(
      fs::read_dir(self.numbers_dir())?
        .filter_map(|dir_entry| -> Option<u64> {
          u64::from_str(dir_entry.ok()?.path().file_stem()?.to_str()?).ok()
        })
        .collect(),
    )
  }

  fn add_progress_bar<Iter, T>(message: &str, iterator: Iter) -> Box<(dyn Iterator<Item = T>)>
  where
    Iter: Iterator<Item = T> + ExactSizeIterator + 'static,
  {
    if cfg!(test) || log_enabled!(log::Level::Info) || integration_test() {
      Box::new(iterator)
    } else {
      Box::new(iterator.into_iter().progress_with_style(
        ProgressStyle::with_template(&format!("[{message}] {{wide_bar}} {{pos}}/{{len}}")).unwrap(),
      ))
    }
  }

  fn export_inscription_by_id(
    &self,
    index: &Arc<Index>,
    id: InscriptionId,
    entry: InscriptionEntry,
  ) -> Result {
    let inscription = index
      .get_inscription_by_id(id)?
      .ok_or_else(|| anyhow!("inscription not found: {id}"))?;
    let number = entry.number;
    match inscription.body() {
      None => log::info!("inscription {number} has no body"),
      Some(body) => {
        let content_type = inscription
          .content_type()
          .ok_or_else(|| anyhow!("content_type missing for {number}"))?;
        let file = match Media::extension_for_content_type(content_type) {
          Err(message) => {
            log::info!("{message}, writing inscription {number} without file extension");
            self.numbers_dir().join(number.to_string())
          }
          Ok(extension) => self.numbers_dir().join(format!("{number}.{extension}")),
        };
        fs::write(file, body)?;
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod test {
  use {
    super::*,
    crate::{
      index::{entry::InscriptionEntry, tests::Context},
      test::{inscription, InscriptionId},
    },
    pretty_assertions::assert_eq,
    test_bitcoincore_rpc::TransactionTemplate,
  };

  impl Context {
    fn export_dir(&self) -> PathBuf {
      self.tempdir.path().join("inscriptions")
    }

    fn write_test_inscription(
      &self,
      input: usize,
      inscription: Inscription,
    ) -> Result<InscriptionEntry> {
      self.mine_blocks(1);
      let txid = self.rpc_server.broadcast_tx(TransactionTemplate {
        inputs: &[(input, 0, 0)],
        witness: inscription.to_witness(),
        ..Default::default()
      });
      let inscription_id = InscriptionId::from(txid);
      self.mine_blocks(1);
      let entry = self
        .index()
        .get_inscription_entry(inscription_id)?
        .ok_or_else(|| anyhow!("no entry for {inscription_id}"))?;
      Ok(entry)
    }
  }

  #[test]
  fn write_inscriptions_to_disk_by_number() -> Result {
    let context = Context::builder().build();
    let entry =
      context.write_test_inscription(1, inscription("text/plain;charset=utf-8", "foo"))?;
    let export = Export {
      output_dir: context.export_dir(),
    };
    export.run_with_index(context.index())?;
    assert_eq!(
      fs::read_to_string(
        context
          .export_dir()
          .join(format!("numbers/{}.txt", entry.number))
      )?,
      "foo"
    );
    Ok(())
  }

  #[test]
  fn write_multiple_inscriptions() -> Result {
    let context = Context::builder().build();
    let contents = vec!["foo", "bar"];
    let entries = contents
      .iter()
      .enumerate()
      .map(|(i, contents)| {
        context.write_test_inscription(i + 1, inscription("text/plain;charset=utf-8", contents))
      })
      .collect::<Result<Vec<InscriptionEntry>>>()?;
    let export = Export {
      output_dir: context.export_dir(),
    };
    export.run_with_index(context.index())?;
    for (expected_contents, entry) in contents.iter().zip(entries) {
      let file = context
        .export_dir()
        .join(format!("numbers/{}.txt", entry.number));
      assert_eq!(
        &anyhow::Context::context(fs::read_to_string(&file), format!("file: {file:?}"))?,
        *expected_contents,
      );
    }
    Ok(())
  }

  #[test]
  fn other_content_types() -> Result {
    let context = Context::builder().build();
    let entry = context.write_test_inscription(1, inscription("application/json", "{}"))?;
    let export = Export {
      output_dir: context.export_dir(),
    };
    export.run_with_index(context.index())?;
    assert_eq!(
      fs::read_to_string(
        context
          .export_dir()
          .join("numbers")
          .join(format!("{}.json", entry.number))
      )?,
      "{}"
    );
    Ok(())
  }

  #[test]
  fn write_unsupported_content_types_without_extensions() -> Result {
    let context = Context::builder().build();
    let entry = context.write_test_inscription(1, inscription("something unsupported", "foo"))?;
    let export = Export {
      output_dir: context.export_dir(),
    };
    export.run_with_index(context.index())?;
    assert_eq!(
      fs::read_to_string(
        context
          .export_dir()
          .join("numbers")
          .join(format!("{}", entry.number))
      )?,
      "foo"
    );
    Ok(())
  }

  #[test]
  fn content_types_without_charset_parameter() -> Result {
    let context = Context::builder().build();
    let text_entry = context.write_test_inscription(1, inscription("text/plain", "foo"))?;
    let html_entry = context.write_test_inscription(2, inscription("text/html", "<foo/>"))?;
    let export = Export {
      output_dir: context.export_dir(),
    };
    export.run_with_index(context.index())?;
    assert_eq!(
      fs::read_to_string(
        context
          .export_dir()
          .join("numbers")
          .join(format!("{}.txt", text_entry.number))
      )?,
      "foo"
    );
    assert_eq!(
      fs::read_to_string(
        context
          .export_dir()
          .join("numbers")
          .join(format!("{}.html", html_entry.number))
      )?,
      "<foo/>"
    );
    Ok(())
  }

  #[test]
  fn handle_inscriptions_without_bodies_gracefully() -> Result {
    let context = Context::builder().build();
    context.write_test_inscription(
      1,
      Inscription::new(Some("text/plain;charset=utf-8".into()), None),
    )?;
    let export = Export {
      output_dir: context.export_dir(),
    };
    export.run_with_index(context.index())?;
    Ok(())
  }

  #[test]
  fn abort_gracefully_on_ctrl_c() -> Result {
    let context = Context::builder().build();
    let n = 100;
    for i in 0..n {
      context.write_test_inscription(i + 1, inscription("text/plain;charset=utf-8", "foo"))?;
    }
    let thread = {
      let export = Export {
        output_dir: context.export_dir(),
      };
      let index = context.index();
      thread::spawn(move || -> Result {
        export.run_with_index(index)?;
        Ok(())
      })
    };
    SHUTTING_DOWN.store(true, atomic::Ordering::Relaxed);
    thread.join().unwrap()?;
    let written_files = fs::read_dir(context.export_dir())?.count();
    assert!(written_files > 0, "no inscriptions written");
    assert!(written_files < n, "all {n} inscriptions written");
    Ok(())
  }

  #[test]
  fn avoid_rewriting_existing_files() -> Result {
    let context = Context::builder().build();
    let entry = context.write_test_inscription(1, inscription("text/plain", "foo"))?;
    let export = Export {
      output_dir: context.export_dir(),
    };
    export.run_with_index(context.index())?;
    let file = context
      .export_dir()
      .join("numbers")
      .join(format!("{}.txt", entry.number));
    fs::write(&file, "bar")?;
    export.run_with_index(context.index())?;
    assert_eq!(fs::read_to_string(file)?, "bar");
    Ok(())
  }
}
