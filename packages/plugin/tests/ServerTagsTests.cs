using Ankor.Core;
using Xunit;

namespace Ankor.Tests;

public sealed class ServerTagsTests
{
    private const string AllocationId = "0199a0b4-5c00-7000-8000-00000000b001";
    private const string Tag = "0199a0b45c007000800000000000b001";

    [Fact]
    public void TagsTheServerWithItsAllocationIdWithoutDashes()
    {
        Assert.Equal(Tag, ServerTags.Tag(AllocationId));
        Assert.Equal(Tag, ServerTags.With(null, AllocationId));
        Assert.Equal(Tag, ServerTags.With(" , ", AllocationId));
    }

    [Fact]
    public void KeepsTheTagsAlreadySetAheadOfTheAllocationsTag()
    {
        Assert.Equal($"league,eu,{Tag}", ServerTags.With("league, eu", AllocationId));
    }

    [Fact]
    public void LeavesTagsThatAlreadyCarryTheAllocationsTagAlone()
    {
        Assert.Null(ServerTags.With($"league,{Tag}", AllocationId));
    }

    [Fact]
    public void DropsTheTagsSetLastWhenTheyLeaveNoRoomForTheAllocationsTag()
    {
        var first = new string('a', 60);
        var second = new string('b', 30);

        var tags = ServerTags.With($"{first},{second}", AllocationId);

        Assert.Equal($"{first},{Tag}", tags);
        Assert.True(tags!.Length <= ServerTags.MaxLength);
    }
}
