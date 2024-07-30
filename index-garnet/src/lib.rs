use drasi_core::interface::IndexError;
use redis::{aio::MultiplexedConnection, cmd, AsyncCommands};

pub mod element_index;
pub mod future_queue;
pub mod result_index;
mod storage_models;

#[cfg(test)]
mod tests;

trait ClearByPattern {
    async fn clear(&self, pattern: String) -> Result<(), IndexError>;
}

impl ClearByPattern for MultiplexedConnection {
    async fn clear(&self, pattern: String) -> Result<(), IndexError> {
        let mut con = self.clone();
        let mut con2 = self.clone();

        let mut cursor = "0".to_string();
        loop {
            let mut cmd = cmd("SCAN");
            let cmd = cmd.arg(remove_surrounding_quotes(&cursor));
            let cmd = cmd.arg("MATCH");
            let cmd = cmd.arg(&pattern);
            let cmd = cmd.arg("COUNT");
            let cmd = cmd.arg(100);

            let result = match cmd
                .query_async::<MultiplexedConnection, Vec<redis::Value>>(&mut con)
                .await
            {
                Ok(v) => v,
                Err(e) => return Err(IndexError::other(e)),
            };

            if result.len() < 2 {
                break;
            }

            match &result[0] {
                redis::Value::Status(s) => {
                    cursor = s.clone();
                }
                redis::Value::Data(d) => match String::from_utf8(d.to_vec()) {
                    Ok(s) => {
                        cursor = s;
                    }
                    Err(_) => (),
                },
                _ => (),
            }

            match &result[1] {
                redis::Value::Bulk(b) => {
                    for k in b {
                        match k {
                            redis::Value::Data(d) => match String::from_utf8(d.to_vec()) {
                                Ok(k) => {
                                    match con2.del::<&str, ()>(remove_surrounding_quotes(&k)).await
                                    {
                                        Ok(_) => (),
                                        Err(e) => return Err(IndexError::other(e)),
                                    }
                                }
                                Err(_) => (),
                            },
                            _ => (),
                        }
                    }
                }
                _ => (),
            }

            if cursor == "0" {
                break;
            }
        }
        Ok(())
    }
}

fn remove_surrounding_quotes(s: &str) -> &str {
    if s.len() >= 2
        && (s.starts_with('"') && s.ends_with('"') || s.starts_with('\'') && s.ends_with('\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}
