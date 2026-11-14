namespace Ankor.Core;

/// <summary>
/// The chat commands players type, without the framework's <c>!</c> prefix. The adapter registers
/// them, and chat messages that tell players what to type take them as arguments.
/// </summary>
public static class ChatCommands
{
    public const string Stay = "stay";
    public const string Swap = "swap";
    public const string Switch = "switch";
    public const string Ct = "ct";
    public const string T = "t";
    public const string ForceReady = "forceready";
    public const string Ready = "ready";
    public const string ReadyShort = "r";
    public const string Unready = "unready";
    public const string NotReady = "notready";
    public const string UnreadyShort = "ur";
}
