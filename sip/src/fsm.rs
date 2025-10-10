use super::message::Method;
use super::transaction::{Transaction, TransactionType};
use super::transport::TransportType;
use anyhow::{Error, Result};
use std::time::Duration;
use strum_macros;
use strum_macros::EnumString;
use thiserror::Error;

const T1: Duration = Duration::from_millis(150);
const T2: Duration = Duration::from_secs(4);
const T4: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum FsmError {
    #[error("invalid state")]
    InvalidState,
    #[error("invalid input")]
    InvalidInput,
    #[error("invalid transaction")]
    InvalidTransaction,
}

#[derive(PartialEq, Clone, strum_macros::Display)]
pub enum Input {
    No,

    Req,
    Ack,
    Resp1xx,
    Resp2xx,
    Resp300to699,

    TimerA,
    TimerB,
    TimerD,
    TimerM,
    TimerCancel,

    TimerE,
    TimerF,
    TimerK,
    TimerJ,

    TimerG,
    TimerH,
    TimerI,
    TimerL,

    TransportError,
}

#[derive(strum_macros::Display, EnumString, PartialEq, Eq)]
pub enum State {
    Trying,
    Calling,
    Proceeding,
    Accepted,
    Completed,
    Confirmed,
    Terminated,
}

pub async fn spin(tx: &Transaction, input: Input) -> Result<(), Error> {
    let mutex = tx.new_mutex();
    mutex.lock().await;

    loop {
        let state = tx.get_state().await?;
        let (new_state, input) = match &tx.key.method {
            &Method::INVITE => match tx.key.tx_type {
                TransactionType::Server => {
                    InviteServer::action(tx, &state, &input).await
                }
                TransactionType::Client => {
                    InviteClient::action(tx, &state, &input).await
                }
            },
            _ => match tx.key.tx_type {
                TransactionType::Server => {
                    NonInviteServer::action(tx, &state, &input).await
                }
                TransactionType::Client => {
                    NonInviteClient::action(tx, &state, &input).await
                }
            },
        }?;
        if new_state == State::Terminated {
            tx.delete().await?;
            return Ok(());
        }
        tx.set_state(&new_state).await?;
        if input == Input::No {
            break;
        }
    }

    Ok(())
}

//                                    |INVITE from TU
//                  Timer A fires     |INVITE sent      Timer B fires
//                  Reset A,          V                 or Transport Err.
//                  INVITE sent +-----------+           inform TU
//                    +---------|           |--------------------------+
//                    |         |  Calling  |                          |
//                    +-------->|           |-----------+              |
//   300-699                    +-----------+ 2xx       |              |
//   ACK sent                      |  |       2xx to TU |              |
//   resp. to TU                   |  |1xx              |              |
//   +-----------------------------+  |1xx to TU        |              |
//   |                                |                 |              |
//   |                1xx             V                 |              |
//   |                1xx to TU +-----------+           |              |
//   |                +---------|           |           |              |
//   |                |         |Proceeding |           |              |
//   |                +-------->|           |           |              |
//   |                          +-----------+ 2xx       |              |
//   |         300-699             |    |     2xx to TU |              |
//   |         ACK sent,  +--------+    +---------------+              |
//   |         resp. to TU|                             |              |
//   |                    |                             |              |
//   |                    V                             V              |
//   |              +-----------+                   +----------+       |
//   +------------->|           |Transport Err.     |          |       |
//                  | Completed |Inform TU          | Accepted |       |
//               +--|           |-------+           |          |-+     |
//       300-699 |  +-----------+       |           +----------+ |     |
//       ACK sent|    ^  |              |               |  ^     |     |
//               |    |  |              |               |  |     |     |
//               +----+  |              |               |  +-----+     |
//                       |Timer D fires |  Timer M fires|    2xx       |
//                       |-             |             - |    2xx to TU |
//                       +--------+     |   +-----------+              |
//      NOTE:                     V     V   V                          |
//   Transitions                 +------------+                        |
//   are labeled                 |            |                        |
//   with the event              | Terminated |<-----------------------+
//   over the action             |            |
//   to take.                    +------------+
//
//                               INVITE client transaction
#[derive(Default, Debug)]
pub struct InviteClient;

async fn client_passup(tx: &Transaction) -> Input {
    let _ = tx.passup().await;
    Input::No
}

// async fn client_resend(tx: &Transaction) -> Input {
//     tx.resend().await;
//     Input::No
// }

fn client_timeout(_tx: &Transaction) -> Input {
    Input::No
}

fn client_transport_error(_tx: &Transaction) -> Input {
    Input::No
}

fn no_action() -> Input {
    Input::No
}

impl InviteClient {
    pub async fn action(
        tx: &Transaction,
        state: &State,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match state {
            State::Calling => Self::calling_act(tx, input).await,
            State::Proceeding => Self::proceeding_act(tx, input).await,
            State::Accepted => Self::accepted_act(tx, input).await,
            State::Completed => Self::completed_act(tx, input).await,
            _ => Err(FsmError::InvalidState)?,
        }
    }

    async fn calling_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Resp1xx => Ok((State::Proceeding, client_passup(tx).await)),
            Input::Resp2xx => Ok((State::Accepted, Self::act_200(tx).await)),
            Input::Resp300to699 => Ok((State::Completed, Self::act_300(tx).await)),
            Input::TimerA => Ok((State::Calling, Self::client_resend(tx).await)),
            Input::TimerB => Ok((State::Terminated, client_timeout(tx))),
            Input::TimerCancel => {
                Ok((State::Calling, Self::client_resend_cancel_wait(tx).await))
            }
            Input::TransportError => {
                Ok((State::Terminated, client_transport_error(tx)))
            }
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn proceeding_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Resp1xx => Ok((State::Proceeding, client_passup(tx).await)),
            Input::Resp2xx => Ok((State::Accepted, Self::act_200(tx).await)),
            Input::Resp300to699 => Ok((State::Completed, Self::act_300(tx).await)),
            Input::TimerCancel => {
                Ok((State::Proceeding, Self::client_resend_cancel(tx).await))
            }
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn accepted_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Resp2xx => Ok((State::Accepted, client_passup(tx).await)),
            Input::TimerM => Ok((State::Terminated, no_action())),
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn completed_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Resp300to699 => Ok((State::Accepted, Self::ack(tx).await)),
            Input::TimerD => Ok((State::Terminated, no_action())),
            Input::TransportError => {
                Ok((State::Terminated, client_transport_error(tx)))
            }
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn client_resend(tx: &Transaction) -> Input {
        let _ = tx.resend().await;
        let resend_count = tx.incr_resend().await.unwrap_or(0);

        let mut duration = T1;
        for _ in 0..resend_count {
            duration = duration * 2;
        }
        tx.register_timer(duration, Input::TimerA);
        Input::No
    }

    async fn client_resend_cancel_wait(tx: &Transaction) -> Input {
        tx.register_timer(4 * T1, Input::TimerCancel);
        Input::No
    }

    async fn client_resend_cancel(tx: &Transaction) -> Input {
        let _ = tx.resend().await;
        let resend_count = tx.incr_resend().await.unwrap_or(0);

        let mut duration = 4 * T1;
        for _ in 0..resend_count {
            duration = duration * 2;
        }
        tx.register_timer(duration, Input::TimerCancel);

        Input::No
    }

    async fn act_200(tx: &Transaction) -> Input {
        client_passup(tx).await;
        tx.register_timer(64 * T1, Input::TimerM);
        Input::No
    }

    async fn act_300(tx: &Transaction) -> Input {
        println!("now invite client tx act 300");
        client_passup(tx).await;
        Self::ack(tx).await;
        if let Ok(uri) = tx.get_remote_uri().await {
            let duration = if uri.transport == TransportType::Udp {
                Duration::from_secs(32)
            } else {
                Duration::from_secs(0)
            };
            tx.register_timer(duration, Input::TimerD);
        }
        Input::No
    }

    async fn ack(tx: &Transaction) -> Input {
        let _ = tx.ack().await;
        Input::No
    }
}

//                                      |INVITE
//                                      |pass INV to TU
//                   INVITE             V send 100 if TU won't in 200 ms
//                   send response+------------+
//                       +--------|            |--------+ 101-199 from TU
//                       |        |            |        | send response
//                       +------->|            |<-------+
//                                | Proceeding |
//                                |            |--------+ Transport Err.
//                                |            |        | Inform TU
//                                |            |<-------+
//                                +------------+
//                   300-699 from TU |    |2xx from TU
//                   send response   |    |send response
//                    +--------------+    +------------+
//                    |                                |
//   INVITE           V          Timer G fires         |
//   send response +-----------+ send response         |
//        +--------|           |--------+              |
//        |        |           |        |              |
//        +------->| Completed |<-------+      INVITE  |  Transport Err.
//                 |           |               -       |  Inform TU
//        +--------|           |----+          +-----+ |  +---+
//        |        +-----------+    | ACK      |     | v  |   v
//        |          ^   |          | -        |  +------------+
//        |          |   |          |          |  |            |---+ ACK
//        +----------+   |          |          +->|  Accepted  |   | to TU
//        Transport Err. |          |             |            |<--+
//        Inform TU      |          V             +------------+
//                       |      +-----------+        |  ^     |
//                       |      |           |        |  |     |
//                       |      | Confirmed |        |  +-----+
//                       |      |           |        |  2xx from TU
//         Timer H fires |      +-----------+        |  send response
//         -             |          |                |
//                       |          | Timer I fires  |
//                       |          | -              | Timer L fires
//                       |          V                | -
//                       |        +------------+     |
//                       |        |            |<----+
//                       +------->| Terminated |
//                                |            |
//                                +------------+
//
//                               INVITE server transaction
#[derive(Default, Debug)]
pub struct InviteServer;

async fn server_act_200(tx: &Transaction) -> Input {
    println!("server act 200");
    let _ = tx.reply().await;
    tx.register_timer(64 * T1, Input::TimerL);
    Input::No
}

async fn server_act_300(tx: &Transaction) -> Input {
    let _ = tx.reply().await;

    if let Ok(uri) = tx.get_remote_uri().await {
        if uri.transport == TransportType::Udp {
            tx.register_timer(T1, Input::TimerG);
        }
    }

    tx.register_timer(64 * T1, Input::TimerH);

    Input::No
}

async fn server_respond(tx: &Transaction) -> Input {
    let _ = tx.reply().await;
    Input::No
}

async fn server_ack(tx: &Transaction) -> Input {
    if let Ok(uri) = tx.get_remote_uri().await {
        let duration = if uri.transport == TransportType::Udp {
            T4
        } else {
            Duration::from_secs(0)
        };
        tx.register_timer(duration, Input::TimerI);
    }
    Input::No
}

fn server_passup(_tx: &Transaction) -> Input {
    Input::No
}

fn server_timeout(_tx: &Transaction) -> Input {
    Input::No
}

fn server_transport_error(_tx: &Transaction) -> Input {
    Input::No
}

impl InviteServer {
    pub async fn action(
        tx: &Transaction,
        state: &State,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match state {
            State::Proceeding => Self::proceeding_act(tx, input).await,
            State::Completed => Self::completed_act(tx, input).await,
            State::Confirmed => Self::confirmed_act(tx, input),
            State::Accepted => Self::accepted_act(tx, input).await,
            _ => Err(FsmError::InvalidState)?,
        }
    }

    async fn proceeding_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Req => Ok((State::Proceeding, server_respond(tx).await)),
            Input::Resp1xx => Ok((State::Proceeding, server_respond(tx).await)),
            Input::TransportError => {
                Ok((State::Proceeding, server_transport_error(tx)))
            }
            Input::Resp2xx => Ok((State::Accepted, server_act_200(tx).await)),
            Input::Resp300to699 => Ok((State::Completed, server_act_300(tx).await)),
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn completed_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Req => Ok((State::Completed, server_respond(tx).await)),
            Input::TimerG => Ok((State::Completed, server_respond(tx).await)),
            Input::TransportError => {
                Ok((State::Completed, server_transport_error(tx)))
            }
            Input::Ack => Ok((State::Confirmed, server_ack(tx).await)),
            Input::TimerH => Ok((State::Terminated, server_timeout(tx))),
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn accepted_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Req => Ok((State::Accepted, no_action())),
            Input::Resp2xx => Ok((State::Accepted, server_respond(tx).await)),
            Input::Ack => Ok((State::Accepted, server_passup(tx))),
            Input::TransportError => {
                Ok((State::Accepted, server_transport_error(tx)))
            }
            Input::TimerL => Ok((State::Terminated, no_action())),
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    fn confirmed_act(
        _tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::TimerI => Ok((State::Terminated, no_action())),
            _ => Err(FsmError::InvalidInput)?,
        }
    }
}

//                                   |Request from TU
//                                   |send request
//               Timer E             V
//               send request  +-----------+
//                   +---------|           |-------------------+
//                   |         |  Trying   |  Timer F          |
//                   +-------->|           |  or Transport Err.|
//                             +-----------+  inform TU        |
//                200-699         |  |                         |
//                resp. to TU     |  |1xx                      |
//                +---------------+  |resp. to TU              |
//                |                  |                         |
//                |   Timer E        V       Timer F           |
//                |   send req +-----------+ or Transport Err. |
//                |  +---------|           | inform TU         |
//                |  |         |Proceeding |------------------>|
//                |  +-------->|           |-----+             |
//                |            +-----------+     |1xx          |
//                |              |      ^        |resp to TU   |
//                | 200-699      |      +--------+             |
//                | resp. to TU  |                             |
//                |              |                             |
//                |              V                             |
//                |            +-----------+                   |
//                |            |           |                   |
//                |            | Completed |                   |
//                |            |           |                   |
//                |            +-----------+                   |
//                |              ^   |                         |
//                |              |   | Timer K                 |
//                +--------------+   | -                       |
//                                   |                         |
//                                   V                         |
//             NOTE:           +-----------+                   |
//                             |           |                   |
//         transitions         | Terminated|<------------------+
//         labeled with        |           |
//         the event           +-----------+
//         over the action
//         to take
//
//                            non-INVITE client transaction
#[derive(Default, Debug)]
pub struct NonInviteClient;

impl NonInviteClient {
    pub async fn action(
        tx: &Transaction,
        state: &State,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match state {
            State::Trying => Self::trying_act(tx, input).await,
            State::Proceeding => Self::proceeding_act(tx, input).await,
            State::Completed => Self::completed_act(tx, input),
            _ => Err(FsmError::InvalidState)?,
        }
    }

    async fn trying_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::TimerE => {
                Ok((State::Trying, Self::client_trying_resend(tx).await))
            }
            Input::Resp1xx => Ok((State::Proceeding, client_passup(tx).await)),
            Input::Resp2xx => Ok((State::Completed, Self::act_200to699(tx).await)),
            Input::Resp300to699 => {
                Ok((State::Completed, Self::act_200to699(tx).await))
            }
            Input::TimerF => Ok((State::Terminated, client_timeout(tx))),
            Input::TransportError => {
                Ok((State::Terminated, client_transport_error(tx)))
            }
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn proceeding_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::TimerE => {
                Ok((State::Proceeding, Self::client_proceeding_resend(tx).await))
            }
            Input::Resp1xx => Ok((State::Proceeding, client_passup(tx).await)),
            Input::Resp2xx => Ok((State::Completed, Self::act_200to699(tx).await)),
            Input::Resp300to699 => {
                Ok((State::Completed, Self::act_200to699(tx).await))
            }
            Input::TimerF => Ok((State::Terminated, client_timeout(tx))),
            Input::TransportError => {
                Ok((State::Terminated, client_transport_error(tx)))
            }
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    fn completed_act(
        _tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::TimerK => Ok((State::Terminated, no_action())),
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn client_trying_resend(tx: &Transaction) -> Input {
        let _ = tx.resend().await;
        let resend_count = tx.incr_resend().await.unwrap_or(0);

        let mut duration = T1;
        for _ in 0..resend_count {
            duration *= 2;
        }
        duration = duration.min(T2);
        tx.register_timer(duration, Input::TimerE);
        Input::No
    }

    async fn client_proceeding_resend(tx: &Transaction) -> Input {
        let _ = tx.resend().await;
        tx.register_timer(T2, Input::TimerE);
        Input::No
    }

    async fn act_200to699(tx: &Transaction) -> Input {
        client_passup(tx).await;
        if let Ok(uri) = tx.get_remote_uri().await {
            let duration = if uri.transport == TransportType::Udp {
                T4
            } else {
                Duration::from_secs(0)
            };
            tx.register_timer(duration, Input::TimerK);
        }
        Input::No
    }
}

//                                  |Request received
//                                  |pass to TU
//                                  V
//                            +-----------+
//                            |           |
//                            | Trying    |-------------+
//                            |           |             |
//                            +-----------+             |200-699 from TU
//                                  |                   |send response
//                                  |1xx from TU        |
//                                  |send response      |
//                                  |                   |
//               Request            V      1xx from TU  |
//               send response+-----------+send response|
//                   +--------|           |--------+    |
//                   |        | Proceeding|        |    |
//                   +------->|           |<-------+    |
//            +<--------------|           |             |
//            |Trnsprt Err    +-----------+             |
//            |Inform TU            |                   |
//            |                     |                   |
//            |                     |200-699 from TU    |
//            |                     |send response      |
//            |  Request            V                   |
//            |  send response+-----------+             |
//            |      +--------|           |             |
//            |      |        | Completed |<------------+
//            |      +------->|           |
//            +<--------------|           |
//            |Trnsprt Err    +-----------+
//            |Inform TU            |
//            |                     |Timer J fires
//            |                     |-
//            |                     |
//            |                     V
//            |               +-----------+
//            |               |           |
//            +-------------->| Terminated|
//                            |           |
//                            +-----------+
//
//                          non-INVITE server transaction
#[derive(Default, Debug)]
pub struct NonInviteServer;

impl NonInviteServer {
    pub async fn action(
        tx: &Transaction,
        state: &State,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match state {
            State::Trying => Self::trying_act(tx, input).await,
            State::Proceeding => Self::proceeding_act(tx, input).await,
            State::Completed => Self::completed_act(tx, input).await,
            _ => Err(FsmError::InvalidState)?,
        }
    }

    async fn trying_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Resp1xx => Ok((State::Proceeding, server_respond(tx).await)),
            Input::Resp2xx => Ok((State::Completed, Self::act_200to699(tx).await)),
            Input::Resp300to699 => {
                Ok((State::Completed, Self::act_200to699(tx).await))
            }
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn proceeding_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Req => Ok((State::Proceeding, server_respond(tx).await)),
            Input::Resp1xx => Ok((State::Proceeding, server_respond(tx).await)),
            Input::Resp2xx => Ok((State::Completed, Self::act_200to699(tx).await)),
            Input::Resp300to699 => {
                Ok((State::Completed, Self::act_200to699(tx).await))
            }
            Input::TransportError => {
                Ok((State::Terminated, server_transport_error(tx)))
            }
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn completed_act(
        tx: &Transaction,
        input: &Input,
    ) -> Result<(State, Input), Error> {
        match input {
            Input::Req => Ok((State::Proceeding, server_respond(tx).await)),
            Input::TransportError => {
                Ok((State::Terminated, server_transport_error(tx)))
            }
            Input::TimerJ => Ok((State::Terminated, no_action())),
            _ => Err(FsmError::InvalidInput)?,
        }
    }

    async fn act_200to699(tx: &Transaction) -> Input {
        if tx.reply().await.is_err() {
            return Input::TransportError;
        }

        if let Ok(uri) = tx.get_remote_uri().await {
            let duration = if uri.transport == TransportType::Udp {
                64 * T1
            } else {
                Duration::from_secs(0)
            };
            tx.register_timer(duration, Input::TimerJ);
        }
        Input::No
    }
}
