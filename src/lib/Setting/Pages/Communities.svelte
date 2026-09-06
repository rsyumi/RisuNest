<script lang="ts">
    import { language } from "src/lang";
    import { openURL } from "src/ts/globalApi.svelte";
</script>

<h2 class="mb-2 text-2xl font-bold mt-2">{language.community}</h2>
<button
    onclick={async () => {openURL("https://github.com/rsyumi/RisuNest")}}
    class="drop-shadow-lg p-3 border-darkbutton border-solid mt-2 flex justify-center items-center ml-2 mr-2 border-1 hover:bg-selected text-sm">
    Github
</button>
