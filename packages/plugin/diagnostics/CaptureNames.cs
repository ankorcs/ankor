namespace Ankor.Diagnostics;

/// <summary>
/// The names a capture file records, shared by the writers (<see cref="DebugCapture"/>,
/// <see cref="GameplayRecording"/> and the adapter) and the replay reader. A captured file keeps
/// these names, so renaming one stops older captures from replaying.
/// </summary>
public static class CaptureNames
{
    /// <summary>The <c>phase</c> of each record.</summary>
    public static class Phase
    {
        public const string Start = "capture_start";
        public const string Enter = "enter";
        public const string Observation = "observation";
        public const string Exit = "exit";
        public const string End = "capture_end";
    }

    /// <summary>The <c>name</c> of the observations that replay reads.</summary>
    public static class Gameplay
    {
        public const string Prefix = "gameplay.";
        public const string Input = Prefix + "input";
        public const string Scheduled = Prefix + "scheduled";
        public const string HostRead = Prefix + "host_read";
    }
}
