using Ankor.Core.Wire;
using Ankor.Tests;

// `constants` prints the plugin's copy of the contract's values for scripts/plugin-contract.mjs.
if (args is ["constants"])
{
    Console.WriteLine(ContractConstants.Json());
    return;
}

if (args.Length != 1) throw new ArgumentException("Expected the output JSONL path, or `constants`");
var result = GameplayScenario.Run();
File.WriteAllLines(args[0], result.Replay.Sent.Select(WireJson.Serialize));
