namespace Ankor.Core;

public static class DisconnectReasons
{
    public static string Name(int reason) => reason switch
    {
        0 => "invalid",
        1 => "shutdown",
        2 => "disconnect_by_user",
        3 => "disconnect_by_server",
        4 => "lost",
        5 => "overflow",
        6 => "steam_banned",
        7 => "steam_inuse",
        8 => "steam_ticket",
        9 => "steam_logon",
        10 => "steam_authcancelled",
        29 => "timedout",
        39 => "kicked",
        40 => "banadded",
        41 => "kickbanadded",
        157 => "kicked_votedoff",
        158 => "kicked_idle",
        _ => reason.ToString(),
    };
}
