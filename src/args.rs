use std::path::PathBuf;

pub enum Command {
    Start(PathBuf),
    Get,
    Set {
        output: Option<String>,
        path: PathBuf,
    },
    Unset(String),
}

impl Command {
    pub fn parse() -> Result<Command, lexopt::Error> {
        use lexopt::prelude::*;

        let mut parser = lexopt::Parser::from_env();
        let cmd = match parser.next()? {
            Some(Value(v)) => v.string()?,
            Some(arg) => return Err(arg.unexpected()),
            None => return Err("missing command".into()),
        };

        let command = match cmd.as_str() {
            "get" => Command::Get,
            "set" => {
                let mut output = None;
                let mut path = None;

                while let Some(arg) = parser.next()? {
                    match arg {
                        Short('o') | Long("output") => output = Some(parser.value()?.string()?),
                        Value(v) if path.is_none() => path = Some(PathBuf::from(v)),
                        _ => return Err(arg.unexpected()),
                    }
                }

                let path = path.ok_or("missing path")?;

                Command::Set { output, path }
            }
            "unset" => match parser.next()? {
                Some(Value(v)) => Command::Unset(v.string()?),
                Some(arg) => return Err(arg.unexpected()),
                None => return Err("missing output name".into()),
            },
            other => Command::Start(other.into()),
        };

        if let Some(arg) = parser.next()? {
            return Err(arg.unexpected());
        }

        Ok(command)
    }

    pub fn print_usage() {
        println!("usage: wbg <path>");
        println!("       wbg set [-o <output>] <path>");
        println!("       wbg unset <output>");
        println!("       wbg get");
    }
}
