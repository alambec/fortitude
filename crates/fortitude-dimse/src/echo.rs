//! Verification service (PS3.4 Annex A): C-ECHO.

use crate::association::{Connection, Error, Message};
use crate::command::status;

pub(crate) async fn handle(connection: &mut Connection, message: Message) -> Result<u16, Error> {
    let response = message.command.response(status::SUCCESS, false);
    connection.send(message.context, &response, None).await?;
    Ok(status::SUCCESS)
}
