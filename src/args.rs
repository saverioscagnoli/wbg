use std::path::PathBuf;

pub enum Command {
    Start(PathBuf),
    Get,
    Set(PathBuf),
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
            "set" => match parser.next()? {
                Some(Value(v)) => Command::Set(v.string()?.into()),
                Some(arg) => return Err(arg.unexpected()),
                None => return Err("missing command".into()),
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
        println!("       wbg set <path>");
        println!("       wbg get");
    }
}
