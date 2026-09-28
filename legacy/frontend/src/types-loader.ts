export type LoaderComponentId =
  | 'net.fabricmc.fabric-loader'
  | 'org.quiltmc.quilt-loader'
  | 'net.minecraftforge'
  | 'net.neoforged';

export interface VersionLoaderAttachment {
  component_id: LoaderComponentId;
  build_id: string;
  loader_version: string;
}
